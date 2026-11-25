//! 控制流帧编码：`4 字节大端长度 | Envelope protobuf 字节`。
//!
//! 8 MiB 上限用于防对端发超大长度 header 造成内存放大；解码侧先校验长度再分配。

use prost::Message as _;

use crate::{consts::MAX_FRAME_LEN, Envelope, ProtoError};

/// 帧长度前缀字节数。
pub const FRAME_LEN_PREFIX: usize = 4;

/// 把一个信封编码成完整帧（含 4B 长度前缀）。
///
/// 返回 [`ProtoError::Oversize`] 如果 protobuf 体超过 [`MAX_FRAME_LEN`]。
pub fn encode_frame(env: &Envelope) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::new();
    encode_frame_into(env, &mut out)?;
    Ok(out)
}

/// 把信封追加编码到 `out`（避免临时分配）。
pub fn encode_frame_into(env: &Envelope, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    let body = env.encode_to_vec();
    if body.len() > MAX_FRAME_LEN {
        return Err(ProtoError::Oversize {
            len: body.len(),
            max: MAX_FRAME_LEN,
        });
    }
    out.reserve(FRAME_LEN_PREFIX + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

/// 只校验并读出帧头声明的长度（流式读取时先读 4B，再按此长度读 body）。
pub fn peek_frame_len(header: &[u8]) -> Result<usize, ProtoError> {
    if header.len() < FRAME_LEN_PREFIX {
        return Err(ProtoError::ShortHeader {
            need: FRAME_LEN_PREFIX,
            got: header.len(),
        });
    }
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::Oversize {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    Ok(len)
}

/// 解码缓冲区里**恰好一帧**（要求长度前缀与剩余字节严格一致）。
pub fn decode_frame(buf: &[u8]) -> Result<Envelope, ProtoError> {
    let len = peek_frame_len(buf)?;
    let available = buf.len() - FRAME_LEN_PREFIX;
    if available != len {
        return Err(ProtoError::LengthMismatch {
            declared: len,
            available,
        });
    }
    Ok(Envelope::decode(&buf[FRAME_LEN_PREFIX..])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{consts::MAX_FRAME_LEN, msg::envelope};

    #[test]
    fn frame_roundtrip() {
        let env = Envelope::hello(1, 0, "客户端 A", vec![0xABu8; 32]);
        let frame = encode_frame(&env).expect("encode");
        assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_be_bytes());
        let got = decode_frame(&frame).expect("decode");
        assert_eq!(got, env);
    }

    #[test]
    fn frame_empty_envelope_roundtrip() {
        let env = Envelope { body: None };
        let frame = encode_frame(&env).unwrap();
        assert_eq!(frame.len(), FRAME_LEN_PREFIX);
        assert_eq!(decode_frame(&frame).unwrap(), env);
    }

    #[test]
    fn frame_rejects_truncated_prefix() {
        let err = decode_frame(&[0, 0, 1]).unwrap_err();
        assert!(
            matches!(err, ProtoError::ShortHeader { need: 4, got: 3 }),
            "{err}"
        );
    }

    #[test]
    fn frame_rejects_oversize_declared_length() {
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 8]);
        let err = decode_frame(&buf).unwrap_err();
        assert!(matches!(err, ProtoError::Oversize { .. }), "{err}");
    }

    #[test]
    fn frame_rejects_length_mismatch() {
        // 声明 100B，实际只有 3B。
        let mut buf = 100u32.to_be_bytes().to_vec();
        buf.extend_from_slice(&[1, 2, 3]);
        let err = decode_frame(&buf).unwrap_err();
        assert!(
            matches!(
                err,
                ProtoError::LengthMismatch {
                    declared: 100,
                    available: 3
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn frame_encode_rejects_oversize_body() {
        // client_name 直接超过 8MiB。
        let env = Envelope::hello(1, 0, "x", vec![0u8; MAX_FRAME_LEN]);
        let err = encode_frame(&env).unwrap_err();
        assert!(matches!(err, ProtoError::Oversize { .. }), "{err}");
    }

    #[test]
    fn frame_decode_rejects_bad_protobuf() {
        let mut buf = 3u32.to_be_bytes().to_vec();
        buf.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(matches!(decode_frame(&buf), Err(ProtoError::Decode(_))));
    }

    #[test]
    fn exact_max_boundary_is_accepted_by_peek() {
        let header = (MAX_FRAME_LEN as u32).to_be_bytes();
        assert_eq!(peek_frame_len(&header).unwrap(), MAX_FRAME_LEN);
        let header = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
        assert!(peek_frame_len(&header).is_err());
    }

    #[test]
    fn encode_frame_into_appends_prefix_and_body() {
        let mut out = vec![0xEEu8; 2];
        let env = Envelope::ping(77);
        encode_frame_into(&env, &mut out).unwrap();
        assert_eq!(&out[..2], &[0xEE, 0xEE]);
        let body_len = out.len() - 2 - FRAME_LEN_PREFIX;
        assert_eq!(&out[2..6], &(body_len as u32).to_be_bytes());
        let got = decode_frame(&out[2..]).unwrap();
        match got.body {
            Some(envelope::Body::Ping(p)) => assert_eq!(p.ts_us, 77),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
