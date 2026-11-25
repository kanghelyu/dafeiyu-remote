// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! H.264 比特流小工具：Annex-B / AVCC 判定、NAL 扫描、SPS/PPS 提取。
//!
//! Media Foundation 的 H.264 编码 MFT 一般输出 **Annex-B**（3 或 4 字节 start
//! code），但个别第三方 MFT 会输出 AVCC（4 字节大端长度前缀）。本模块两种都
//! 能识别，并在需要时把 AVCC 转成 Annex-B，方便上层直接按 Annex-B 传输。

/// 一个 NAL 单元在缓冲区中的范围（不含 start code）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NalRange {
    /// 载荷起始偏移。
    pub start: usize,
    /// 载荷结束偏移（不含）。
    pub end: usize,
}

impl NalRange {
    /// NAL 头字节（为空时返回 0）。
    #[inline]
    pub(crate) fn header(&self, data: &[u8]) -> u8 {
        data.get(self.start).copied().unwrap_or(0)
    }

    /// `nal_unit_type`（H.264 头字节低 5 位）。
    #[inline]
    pub(crate) fn nal_type(&self, data: &[u8]) -> u8 {
        self.header(data) & 0x1f
    }
}

/// 是否为 Annex-B 起始（`00 00 01` 或 `00 00 00 01`）。
pub(crate) fn has_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// 扫描 Annex-B 流中的所有 NAL 载荷范围。
///
/// 容错：忽略 start code 前的填充字节、以及长度为 0 的空 NAL。
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<NalRange> {
    let mut out = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (start code 起点, 载荷起点)
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else if i + 3 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            starts.push((i, i + 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    for (idx, (_, payload_start)) in starts.iter().enumerate() {
        let end = starts
            .get(idx + 1)
            .map(|(sc_start, _)| *sc_start)
            .unwrap_or(data.len());
        // 去掉尾部可能属于下一个 start code 的 0 字节。
        let mut end = end;
        while end > *payload_start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > *payload_start {
            out.push(NalRange {
                start: *payload_start,
                end,
            });
        }
    }
    out
}

/// 一个 NAL 载荷对应的 start code 起始偏移（4 字节或 3 字节）。
fn start_code_pos(data: &[u8], payload_start: usize) -> usize {
    if payload_start >= 4 && data[payload_start - 4..payload_start] == [0, 0, 0, 1] {
        payload_start - 4
    } else {
        payload_start.saturating_sub(3)
    }
}

/// 按 AUD（`nal_unit_type == 9`）把一段 Annex-B 码流切成访问单元。
///
/// Media Foundation 的 H.264 解码 MFT 期望每次 `ProcessInput` 喂**一个访问
/// 单元**；上层常常把整条码流一次性交给 [`crate::VideoDecoder::decode`]，
/// 这里替调用方切分。没有 AUD 时整段视为一个访问单元（兼容单帧输入）。
///
/// 返回 `(起始偏移, 结束偏移)` 列表，覆盖 `data` 的全部字节。
pub(crate) fn annexb_access_units(data: &[u8]) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return Vec::new();
    }
    let nals = annexb_nals(data);
    if nals.len() < 2 {
        return vec![(0, data.len())];
    }
    let mut cuts = Vec::new();
    for nal in nals.iter().skip(1) {
        if nal.nal_type(data) == 9 {
            let sc = start_code_pos(data, nal.start);
            if sc > 0 && cuts.last().copied() != Some(sc) {
                cuts.push(sc);
            }
        }
    }
    if cuts.is_empty() {
        return vec![(0, data.len())];
    }
    let mut units = Vec::with_capacity(cuts.len() + 1);
    let mut begin = 0usize;
    for cut in cuts {
        if cut > begin {
            units.push((begin, cut));
            begin = cut;
        }
    }
    units.push((begin, data.len()));
    units
}

/// 扫描 AVCC 流（4 字节大端长度前缀）中的所有 NAL 载荷范围。
pub(crate) fn avcc_nals(data: &[u8]) -> Option<Vec<NalRange>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        if len == 0 || len > data.len() - i - 4 {
            return None;
        }
        out.push(NalRange {
            start: i + 4,
            end: i + 4 + len,
        });
        i += 4 + len;
    }
    if i == data.len() && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// 猜测比特流是 Annex-B 还是 AVCC。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitstreamKind {
    /// Annex-B（start code 分隔）。
    AnnexB,
    /// AVCC（4 字节长度前缀）。
    Avcc,
}

/// 判定比特流格式；空数据返回 `None`。
pub(crate) fn detect_kind(data: &[u8]) -> Option<BitstreamKind> {
    if data.is_empty() {
        return None;
    }
    if has_start_code(data) {
        return Some(BitstreamKind::AnnexB);
    }
    if avcc_nals(data).is_some() {
        return Some(BitstreamKind::Avcc);
    }
    // 兜底：能扫到 Annex-B start code 就当 Annex-B。
    if !annexb_nals(data).is_empty() {
        Some(BitstreamKind::AnnexB)
    } else {
        None
    }
}

/// 把 AVCC 流转换为 Annex-B（每个 NAL 前加 `00 00 00 01`）。
pub(crate) fn avcc_to_annexb(data: &[u8]) -> Option<Vec<u8>> {
    let nals = avcc_nals(data)?;
    let mut out = Vec::with_capacity(data.len() + nals.len() * 4);
    for n in &nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[n.start..n.end]);
    }
    Some(out)
}

/// 从任意格式的 H.264 数据中提取 SPS（type 7）与 PPS（type 8）NAL 载荷。
pub(crate) fn extract_sps_pps(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let nals = match detect_kind(data) {
        Some(BitstreamKind::AnnexB) => annexb_nals(data),
        Some(BitstreamKind::Avcc) => avcc_nals(data).unwrap_or_default(),
        None => Vec::new(),
    };
    let mut sps = None;
    let mut pps = None;
    for n in &nals {
        match n.nal_type(data) {
            7 if sps.is_none() => sps = Some(data[n.start..n.end].to_vec()),
            8 if pps.is_none() => pps = Some(data[n.start..n.end].to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

/// 数据中是否含 IDR（type 5）或 SPS（type 7）——编码端用它判断关键帧。
pub(crate) fn is_keyframe(data: &[u8]) -> bool {
    let nals = match detect_kind(data) {
        Some(BitstreamKind::AnnexB) => annexb_nals(data),
        Some(BitstreamKind::Avcc) => avcc_nals(data).unwrap_or_default(),
        None => return false,
    };
    nals.iter().any(|n| matches!(n.nal_type(data), 5 | 7))
}

/// 解析 AVCC 配置记录（`MF_MT_MPEG_SEQUENCE_HEADER` / `avcC` 的同构体），
/// 取出 SPS/PPS。
///
/// 布局：version(1) profile(1) compat(1) level(1) lengthSizeMinusOne(1)
/// + numSPS(1, 低 5 位) + [len(2) sps]* + numPPS(1) + [len(2) pps]*。
pub(crate) fn parse_avcc_config(blob: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    if blob.len() < 6 {
        return (None, None);
    }
    let mut i = 5usize;
    let num_sps = (blob[i] & 0x1f) as usize;
    i += 1;
    let mut sps = None;
    for _ in 0..num_sps {
        if i + 2 > blob.len() {
            return (sps, None);
        }
        let len = u16::from_be_bytes([blob[i], blob[i + 1]]) as usize;
        i += 2;
        if i + len > blob.len() {
            break;
        }
        if sps.is_none() {
            sps = Some(blob[i..i + len].to_vec());
        }
        i += len;
    }
    if i >= blob.len() {
        return (sps, None);
    }
    let num_pps = blob[i] as usize;
    i += 1;
    let mut pps = None;
    for _ in 0..num_pps {
        if i + 2 > blob.len() {
            break;
        }
        let len = u16::from_be_bytes([blob[i], blob[i + 1]]) as usize;
        i += 2;
        if i + len > blob.len() {
            break;
        }
        if pps.is_none() {
            pps = Some(blob[i..i + len].to_vec());
        }
        i += len;
    }
    (sps, pps)
}

/// 用 SPS/PPS 生成 AVCC 形式的 codec private（`avcC`），供 mp4/RTP 打包用。
pub(crate) fn build_avcc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    for b in sps.iter().skip(1).take(3) {
        out.push(*b); // profile / compat / level
    }
    out.resize(4, 0);
    out.push(0xff); // lengthSizeMinusOne = 3
    out.push(0xe1); // numSPS = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numPPS = 1
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 Annex-B 流：SPS(1 字节载荷)+PPS+IDR。
    fn annexb_sample() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a, 0x22]);
        v
    }

    #[test]
    fn annexb_scan_and_keyframe() {
        let data = annexb_sample();
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0].nal_type(&data), 7);
        assert_eq!(nals[1].nal_type(&data), 8);
        assert_eq!(nals[2].nal_type(&data), 5);
        assert_eq!(nals[3].nal_type(&data), 1);
        assert!(is_keyframe(&data));
        let (sps, pps) = extract_sps_pps(&data);
        assert_eq!(sps.unwrap(), vec![0x67, 0x42, 0x00, 0x1e]);
        assert_eq!(pps.unwrap(), vec![0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn three_byte_start_codes_work() {
        let data = vec![
            0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[2].nal_type(&data), 5);
        assert!(is_keyframe(&data));
    }

    #[test]
    fn avcc_roundtrip() {
        // 注意：`annexb_nals` 会把 NAL 尾部的 0 字节归一化掉（它们可能属于下一个
        // 4 字节 start code），所以用不以 0 结尾的载荷来做无损往返断言。
        let nals_data: Vec<Vec<u8>> = vec![
            vec![0x67, 0x42, 0x00, 0x1e],
            vec![0x68, 0xce, 0x3c, 0x80],
            vec![0x65, 0x88, 0x84, 0x03],
            vec![0x41, 0x9a, 0x22],
        ];
        let mut annexb = Vec::new();
        let mut avcc = Vec::new();
        for n in &nals_data {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(n);
            avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
            avcc.extend_from_slice(n);
        }
        assert_eq!(detect_kind(&avcc), Some(BitstreamKind::Avcc));
        assert!(is_keyframe(&avcc));
        let back = avcc_to_annexb(&avcc).unwrap();
        assert_eq!(back, annexb);
        let (sps, pps) = extract_sps_pps(&avcc);
        assert_eq!(sps.unwrap(), nals_data[0]);
        assert_eq!(pps.unwrap(), nals_data[1]);
    }

    #[test]
    fn trailing_zero_bytes_are_normalized() {
        // `00 00 00 01` 前的 0 字节无法区分「载荷尾部」与「start code 的一部分」，
        // 统一归到 start code（否则会把下一个 start code 误当载荷）。
        let data = vec![0, 0, 0, 1, 0x67, 0x00, 0, 0, 1, 0x68, 0xce];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(&data[nals[0].start..nals[0].end], &[0x67]);
        assert_eq!(nals[1].nal_type(&data), 8);
    }

    #[test]
    fn parse_mf_sequence_header_blob() {
        // 模拟 avcC：version/profile/compat/level + 4 字节 AVCC 头 + SPS + PPS
        let sps = vec![0x67u8, 0x64, 0x00, 0x1f, 0xac];
        let pps = vec![0x68u8, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
        let mut blob = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        blob.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        blob.extend_from_slice(&sps);
        blob.push(1);
        blob.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        blob.extend_from_slice(&pps);
        let (s, p) = parse_avcc_config(&blob);
        assert_eq!(s.unwrap(), sps);
        assert_eq!(p.unwrap(), pps);
        // build_avcc_config 生成的前缀与 avcC 关键字段一致。
        let built = build_avcc_config(&sps, &pps);
        assert_eq!(built[0], 1);
        assert_eq!(built[4], 0xff);
        assert_eq!(built[5], 0xe1);
        let (s2, p2) = parse_avcc_config(&built);
        assert_eq!(s2.unwrap(), sps);
        assert_eq!(p2.unwrap(), pps);
    }

    #[test]
    fn non_keyframe_and_empty_input() {
        let data = [0u8, 0, 0, 1, 0x41, 0x9a];
        assert!(!is_keyframe(&data));
        assert!(!is_keyframe(&[]));
        assert_eq!(detect_kind(&[]), None);
    }
}
