// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
//! Bounded, display-independent pieces of the ICCCM selection protocol.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};

use crate::DataBlock;

/// A hard ceiling protects callers that configure their own native-backend limit.
pub(crate) const HARD_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_TARGETS: usize = 512;
pub(crate) const TRANSFER_CHUNK: usize = 64 * 1024;
pub(crate) const MAX_TRANSFERS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Latin1,
    Html,
    Png,
}

/// X timestamps wrap after roughly 49 days. Only compare nearby server times.
pub(crate) fn timestamp_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

pub(crate) fn decode(encoding: Encoding, mut bytes: Vec<u8>, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= limit,
        "X11 clipboard payload exceeds byte limit ({limit})"
    );
    match encoding {
        Encoding::Utf8 | Encoding::Html => {
            // Some X applications include a terminal NUL in text properties.
            while bytes.last() == Some(&0) {
                bytes.pop();
            }
            std::str::from_utf8(&bytes).context("X11 clipboard text is not UTF-8")?;
            Ok(bytes)
        }
        Encoding::Latin1 => {
            while bytes.last() == Some(&0) {
                bytes.pop();
            }
            let utf8_len = bytes
                .iter()
                .map(|&b| if b < 128 { 1 } else { 2 })
                .sum::<usize>();
            ensure!(
                utf8_len <= limit,
                "Converted X11 clipboard text exceeds byte limit ({limit})"
            );
            let mut text = String::with_capacity(utf8_len);
            text.extend(bytes.into_iter().map(char::from));
            Ok(text.into_bytes())
        }
        Encoding::Png => Ok(bytes),
    }
}

pub(crate) fn latin1(bytes: &[u8]) -> Option<Arc<[u8]>> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.chars()
        .map(|c| u8::try_from(c as u32).ok())
        .collect::<Option<Vec<_>>>()
        .map(Arc::from)
}

pub(crate) fn block(id: u32, bytes: &[u8], offset: u64, max: usize) -> DataBlock {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = start.saturating_add(max).min(bytes.len());
    DataBlock {
        id,
        total_len: bytes.len() as u64,
        offset,
        block: bytes[start..end].to_vec(),
    }
}

/// The INCR size is a lower bound, not a trusted allocation size. Check every
/// chunk as well, including its X property type and element width.
pub(crate) struct Incoming {
    limit: usize,
    type_and_format: Option<(u32, u8)>,
    bytes: Vec<u8>,
    terminated: bool,
}

impl Incoming {
    pub(crate) fn new(advertised: u32, limit: usize) -> Result<Self> {
        ensure!(
            u64::from(advertised) <= limit as u64,
            "X11 INCR object exceeds byte limit ({limit})"
        );
        Ok(Self {
            limit,
            type_and_format: None,
            bytes: Vec::new(),
            terminated: false,
        })
    }

    /// `true` means the mandatory empty terminator was received.
    pub(crate) fn push(&mut self, type_: u32, format: u8, bytes: &[u8]) -> Result<bool> {
        ensure!(
            !self.terminated,
            "X11 INCR data arrived after the terminator"
        );
        ensure!(
            type_ != 0 && matches!(format, 8 | 16 | 32),
            "Invalid X11 INCR chunk property"
        );
        ensure!(
            bytes.len() % (usize::from(format) / 8) == 0,
            "Misaligned X11 INCR chunk"
        );
        let signature = (type_, format);
        if let Some(previous) = self.type_and_format {
            ensure!(
                previous == signature,
                "X11 INCR property type changed during transfer"
            );
        } else {
            self.type_and_format = Some(signature);
        }
        ensure!(
            bytes.len() <= self.limit.saturating_sub(self.bytes.len()),
            "X11 INCR object exceeds byte limit ({})",
            self.limit
        );
        self.bytes.extend_from_slice(bytes);
        self.terminated = bytes.is_empty();
        Ok(self.terminated)
    }

    pub(crate) fn finish(self) -> Result<(u32, u8, Vec<u8>)> {
        ensure!(self.terminated, "X11 INCR transfer has no terminator");
        let (type_, format) = self
            .type_and_format
            .context("X11 INCR transfer has no property type")?;
        Ok((type_, format, self.bytes))
    }
}

/// Each call corresponds to one PropertyDelete acknowledgement. The final
/// empty property is sent only after the last nonempty chunk was consumed.
pub(crate) struct Outgoing {
    pub(crate) bytes: Arc<[u8]>,
    offset: usize,
    chunk: usize,
    finished: bool,
}

impl Outgoing {
    pub(crate) fn new(bytes: Arc<[u8]>, chunk: usize) -> Result<Self> {
        ensure!(chunk > 0, "X11 INCR chunk size must be positive");
        Ok(Self {
            bytes,
            offset: 0,
            chunk,
            finished: false,
        })
    }

    pub(crate) fn next(&mut self) -> Result<&[u8]> {
        if self.finished {
            bail!("X11 INCR transfer is already complete");
        }
        let start = self.offset;
        self.offset = start.saturating_add(self.chunk).min(self.bytes.len());
        if start == self.bytes.len() {
            self.finished = true;
        }
        Ok(&self.bytes[start..self.offset])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_rejects_declared_and_actual_oversize_without_trusting_hint() {
        assert!(Incoming::new(9, 8).is_err());
        let mut transfer = Incoming::new(1, 8).unwrap();
        assert!(!transfer.push(12, 8, b"123456").unwrap());
        assert!(transfer.push(12, 8, b"789").is_err());
    }

    #[test]
    fn incoming_requires_consistent_type_and_format() {
        let mut transfer = Incoming::new(0, 32).unwrap();
        transfer.push(12, 8, b"abc").unwrap();
        assert!(transfer.push(13, 8, b"d").is_err());
        assert!(transfer.push(12, 16, b"de").is_err());
        assert!(transfer.push(0, 8, b"").is_err());
        assert!(transfer.push(12, 32, b"d").is_err());
        assert!(transfer.push(12, 8, b"").unwrap());
        assert_eq!(transfer.finish().unwrap(), (12, 8, b"abc".to_vec()));
    }

    #[test]
    fn incoming_accepts_many_chunks_but_does_not_preallocate_advertised_size() {
        let mut transfer = Incoming::new(8, 32).unwrap();
        assert_eq!(transfer.bytes.capacity(), 0);
        for part in [b"abcd".as_slice(), b"efgh", b"ijkl"] {
            assert!(!transfer.push(1, 8, part).unwrap());
        }
        assert!(transfer.push(1, 8, b"").unwrap());
        assert_eq!(transfer.finish().unwrap().2, b"abcdefghijkl");
    }

    #[test]
    fn incoming_requires_terminator_and_rejects_data_after_it() {
        assert!(Incoming::new(0, 32).unwrap().finish().is_err());
        let mut unfinished = Incoming::new(3, 32).unwrap();
        unfinished.push(12, 8, b"abc").unwrap();
        assert!(unfinished.finish().is_err());
        let mut finished = Incoming::new(0, 32).unwrap();
        assert!(finished.push(12, 8, b"").unwrap());
        assert!(finished.push(12, 8, b"after").is_err());
        assert!(finished.finish().unwrap().2.is_empty());
    }

    #[test]
    fn outgoing_waits_for_acknowledgement_before_empty_terminator() {
        let mut transfer = Outgoing::new(Arc::from(b"abcdefgh".as_slice()), 4).unwrap();
        assert_eq!(transfer.next().unwrap(), b"abcd");
        assert_eq!(transfer.next().unwrap(), b"efgh");
        assert_eq!(transfer.next().unwrap(), b"");
        assert!(transfer.next().is_err());
        assert!(Outgoing::new(Arc::from([]), 0).is_err());
    }

    #[test]
    fn unicode_and_html_preserve_utf8_and_latin1_expansion_is_bounded() {
        let text = "大肥鱼 🐟\n";
        assert_eq!(
            decode(Encoding::Utf8, text.as_bytes().to_vec(), 128).unwrap(),
            text.as_bytes()
        );
        assert_eq!(
            decode(Encoding::Html, b"<b>fish</b>\0".to_vec(), 128).unwrap(),
            b"<b>fish</b>"
        );
        assert!(decode(Encoding::Utf8, vec![0xff], 128).is_err());
        assert_eq!(
            decode(Encoding::Latin1, vec![0xe9, 0], 3).unwrap(),
            "é".as_bytes()
        );
        assert!(decode(Encoding::Latin1, vec![0xe9, 0xe9], 3).is_err());
        assert_eq!(latin1("café".as_bytes()).unwrap().as_ref(), b"caf\xe9");
        assert!(latin1(text.as_bytes()).is_none());
    }

    #[test]
    fn block_handles_size_probe_end_and_overflow_offsets() {
        assert_eq!(block(12, b"abcdef", 2, 3).block, b"cde");
        let probe = block(12, b"abcdef", 0, 0);
        assert_eq!(probe.total_len, 6);
        assert!(probe.block.is_empty());
        assert!(block(12, b"abc", u64::MAX, usize::MAX).block.is_empty());
        assert_eq!(block(12, b"abc", 1, usize::MAX).block, b"bc");
    }

    #[test]
    fn timestamps_compare_across_server_wrap() {
        assert!(timestamp_before(u32::MAX - 1, 2));
        assert!(!timestamp_before(2, u32::MAX - 1));
        assert!(!timestamp_before(17, 17));
    }
}
