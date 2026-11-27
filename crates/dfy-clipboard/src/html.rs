// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// 大肥鱼远程 —— CF_HTML（"HTML Format"）头部解析与构造。
// CF_HTML 是“ASCII 头部 + UTF-8 HTML 正文”的剪贴板格式；跨端只同步 fragment 片段，
// 由两边各自补齐头部，避免 HTML 文档外壳膨胀。

// 当前仅 Windows 后端/测试使用；其它平台编译时允许未使用（后续 mac/Linux 后端会复用）。
#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::{bail, Result};

/// 注册格式名（`RegisterClipboardFormatW` 用）。
pub const HTML_FORMAT_NAME: &str = "HTML Format";

/// 正文前缀/后缀：把 fragment 包成最小 HTML 文档，并标出 fragment 区间标记。
const BODY_PREFIX: &str = "<!DOCTYPE html><html><body><!--StartFragment-->";
const BODY_SUFFIX: &str = "<!--EndFragment--></body></html>";

/// CF_HTML 头部里我们关心的四个字节偏移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtmlOffsets {
    /// 整个 HTML 文档（正文）起点。
    pub start_html: u64,
    /// 整个 HTML 文档终点。
    pub end_html: u64,
    /// fragment 起点（不含 `<!--StartFragment-->`）。
    pub start_fragment: u64,
    /// fragment 终点（不含 `<!--EndFragment-->`）。
    pub end_fragment: u64,
}

/// 构造完整的 CF_HTML 载荷：头部偏移固定 10 位零填充，保证头部长度恒定。
pub fn encode_html_fragment(fragment: &[u8]) -> Vec<u8> {
    let body_len = BODY_PREFIX.len() + fragment.len() + BODY_SUFFIX.len();
    // 先用 0 算出头部长度（10 位零填充时与真实值同长）。
    let header_len = header(0, 0, 0, 0).len();
    let start_html = header_len as u64;
    let end_html = start_html + body_len as u64;
    let start_fragment = start_html + BODY_PREFIX.len() as u64;
    let end_fragment = start_fragment + fragment.len() as u64;

    let mut out = header(start_html, end_html, start_fragment, end_fragment);
    out.extend_from_slice(BODY_PREFIX.as_bytes());
    out.extend_from_slice(fragment);
    out.extend_from_slice(BODY_SUFFIX.as_bytes());
    out
}

fn header(start_html: u64, end_html: u64, start_fragment: u64, end_fragment: u64) -> Vec<u8> {
    format!(
        "Version:0.9\r\n\
         StartHTML:{start_html:010}\r\n\
         EndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\n\
         EndFragment:{end_fragment:010}\r\n"
    )
    .into_bytes()
}

/// 宽松解析 CF_HTML 头部：找 `Key:` 后第一个十进制数字串。
///
/// 微软规范允许头部字段有空格/前导零差异，浏览器实现也各不相同，所以这里不做严格校验。
pub fn parse_html_offsets(bytes: &[u8]) -> Option<HtmlOffsets> {
    Some(HtmlOffsets {
        start_html: parse_field(bytes, b"StartHTML:")?,
        end_html: parse_field(bytes, b"EndHTML:")?,
        start_fragment: parse_field(bytes, b"StartFragment:")?,
        end_fragment: parse_field(bytes, b"EndFragment:")?,
    })
}

fn parse_field(bytes: &[u8], key: &[u8]) -> Option<u64> {
    let at = find_subslice(bytes, key)? + key.len();
    let mut value = 0u64;
    let mut seen_digit = false;
    for &b in bytes.get(at..)? {
        if b.is_ascii_digit() {
            value = value.checked_mul(10)?.checked_add((b - b'0') as u64)?;
            seen_digit = true;
        } else if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
            // 头部字段名与数字之间可能夹空白；数字结束后遇到换行即结束。
            if seen_digit {
                break;
            }
        } else if seen_digit {
            break;
        } else {
            return None;
        }
    }
    seen_digit.then_some(value)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 提取 fragment 字节（返回的切片可能不含 `<!--StartFragment-->` 标记本身）。
///
/// 偏移非法（越界/顺序颠倒）时，退回 `StartHTML..EndHTML`；再不行返回整个载荷。
pub fn extract_html_fragment(bytes: &[u8]) -> &[u8] {
    if let Some(off) = parse_html_offsets(bytes) {
        let len = bytes.len() as u64;
        if off.start_fragment <= off.end_fragment
            && off.end_fragment <= len
            && off.start_fragment >= off.start_html
        {
            return &bytes[off.start_fragment as usize..off.end_fragment as usize];
        }
        if off.start_html < off.end_html && off.end_html <= len {
            return &bytes[off.start_html as usize..off.end_html as usize];
        }
    }
    bytes
}

/// 构造/解析一组“往返”辅助：把任意 fragment 包成 CF_HTML 再取回。
pub fn wrap_html_fragment(fragment: &[u8]) -> Result<Vec<u8>> {
    let payload = encode_html_fragment(fragment);
    let off = parse_html_offsets(&payload)
        .ok_or_else(|| anyhow::anyhow!("内部错误：自构造的 CF_HTML 头部无法解析"))?;
    if off.end_fragment != payload.len() as u64 - BODY_SUFFIX.len() as u64 {
        bail!("内部错误：CF_HTML 头部偏移与正文不一致");
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_offsets_are_self_consistent() {
        let fragment = "<b>你好</b> dfy".as_bytes();
        let payload = encode_html_fragment(fragment);
        assert!(payload.starts_with(b"Version:0.9\r\n"));
        let off = parse_html_offsets(&payload).expect("header 必须可解析");
        assert_eq!(off.start_html, 105, "10 位零填充头部固定 105 字节");
        assert_eq!(
            off.start_fragment,
            off.start_html + BODY_PREFIX.len() as u64
        );
        assert_eq!(off.end_fragment - off.start_fragment, fragment.len() as u64);
        assert_eq!(
            off.end_html as usize,
            payload.len(),
            "EndHTML 指向整个文档末尾（含正文后缀）"
        );
        assert_eq!(extract_html_fragment(&payload), fragment);
    }

    #[test]
    fn fallback_to_whole_payload_without_header() {
        let raw = b"<p>no header</p>";
        assert_eq!(extract_html_fragment(raw), raw);
    }
}
