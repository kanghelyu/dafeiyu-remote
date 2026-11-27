// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
//! Session credentials: hidden terminal entry or bounded stdin, without secret logging.

use std::fmt;
use std::io::{self, BufRead, IsTerminal};

use anyhow::{ensure, Context, Result};
use clap::Args;
use zeroize::Zeroizing;

const MIN_PASSWORD_BYTES: usize = 16;
const MAX_PASSWORD_BYTES: usize = 1024;

#[derive(Args)]
pub struct PasswordArgs {
    /// Session passphrase (16+ bytes); prefer hidden terminal entry or --password-stdin.
    #[arg(long, value_name = "SECRET", conflicts_with = "password_stdin")]
    password: Option<String>,
    /// Read one passphrase line from stdin, without echoing or saving it.
    #[arg(long)]
    password_stdin: bool,
}

impl fmt::Debug for PasswordArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordArgs([REDACTED])")
    }
}

impl PasswordArgs {
    pub fn read(self) -> Result<SessionPassword> {
        let value = if let Some(password) = self.password {
            Zeroizing::new(password)
        } else if self.password_stdin {
            read_line(io::stdin().lock())?
        } else {
            ensure!(
                io::stdin().is_terminal(),
                "请使用 --password-stdin 提供会话口令，或在交互终端中输入；没有默认口令"
            );
            Zeroizing::new(rpassword::prompt_password(
                "会话口令（至少 16 字节，不回显）: ",
            )?)
        };
        SessionPassword::new(value)
    }
}

/// Heap storage is erased on drop; Debug intentionally never reveals its contents.
#[derive(Clone)]
pub struct SessionPassword(Zeroizing<String>);

impl SessionPassword {
    pub(crate) fn new(value: Zeroizing<String>) -> Result<Self> {
        ensure!(
            (MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&value.len())
                && !value.trim().is_empty(),
            "会话口令长度必须为 16–1024 字节，且不能全为空白"
        );
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SessionPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionPassword([REDACTED])")
    }
}

fn read_line(reader: impl BufRead) -> Result<Zeroizing<String>> {
    let mut value = Zeroizing::new(String::new());
    let bytes = reader
        .take((MAX_PASSWORD_BYTES + 3) as u64)
        .read_line(&mut value)
        .context("读取会话口令失败")?;
    ensure!(bytes != 0, "stdin 未提供会话口令");
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    ensure!(
        value.len() <= MAX_PASSWORD_BYTES,
        "会话口令超过 1024 字节上限"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Cursor;

    #[test]
    fn password_args_have_no_default_and_redact_debug() {
        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            password: PasswordArgs,
        }

        let empty = TestCli::try_parse_from(["password"]).expect("parse empty args");
        assert!(empty.password.password.is_none());
        assert!(!empty.password.password_stdin);

        let secret = "a long secret passphrase";
        let args =
            TestCli::try_parse_from(["password", "--password", secret]).expect("parse password");
        assert_eq!(args.password.password.as_deref(), Some(secret));
        assert!(!format!("{:?}", args.password).contains(secret));
    }

    #[test]
    fn credentials_are_bounded_and_debug_is_redacted() {
        let password = SessionPassword::new(Zeroizing::new("a long secret passphrase".into()))
            .expect("passphrase");
        assert_eq!(password.expose(), "a long secret passphrase");
        assert!(!format!("{password:?}").contains(password.expose()));
        assert!(SessionPassword::new(Zeroizing::new("test".into())).is_err());
        assert!(SessionPassword::new(Zeroizing::new(" ".repeat(16))).is_err());
        assert!(SessionPassword::new(Zeroizing::new("x".repeat(1025))).is_err());
    }

    #[test]
    fn stdin_reads_only_one_line_and_preserves_significant_spaces() {
        let value = read_line(Cursor::new(b"  long passphrase  \r\nignored\n")).expect("line");
        assert_eq!(value.as_str(), "  long passphrase  ");
        assert!(read_line(Cursor::new(Vec::<u8>::new())).is_err());
        assert!(read_line(Cursor::new(vec![b'x'; 4096])).is_err());
    }
}
