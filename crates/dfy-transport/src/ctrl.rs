//! `control` 双向流上的帧收发。
//!
//! 帧格式来自 `dfy_proto`：4 字节大端长度 + Envelope protobuf 字节（上限 8 MiB）。
//! 第一条 `open_bi` 双向流即 control 流（stream id 0），DATAGRAM 走不可靠通道。

use anyhow::{Context, Result};
use dfy_proto::{decode_frame, encode_frame, Envelope, MAX_FRAME_LEN};
use quinn::{Connection, RecvStream, SendStream};
use tracing::trace;

/// 只负责发送的一半（可单独借出，便于在 `tokio::select!` 里同时收/发）。
pub struct CtrlSender {
    send: SendStream,
    peer: Connection,
}

/// 只负责接收的一半。
pub struct CtrlReceiver {
    recv: RecvStream,
}

/// control 流的完整句柄（发送 + 接收）。
pub struct CtrlChannel {
    /// 发送半部。
    pub tx: CtrlSender,
    /// 接收半部。
    pub rx: CtrlReceiver,
}

impl CtrlSender {
    /// 包装一条 quinn 发送流。
    pub fn new(send: SendStream, peer: Connection) -> Self {
        Self { send, peer }
    }

    /// 发送一个 `Envelope`（自动加长度前缀）。
    pub async fn send_ctrl(&mut self, envelope: &Envelope) -> Result<()> {
        let frame = encode_frame(envelope).context("编码 control 帧失败")?;
        trace!(
            kind = envelope.kind_name(),
            bytes = frame.len(),
            "→ control 帧"
        );
        self.send
            .write_all(&frame)
            .await
            .with_context(|| format!("发送 {} 帧失败（control 流已断开？）", envelope.kind_name()))
    }

    /// 发送原始帧字节（已含长度前缀）。
    pub async fn send_frame_bytes(&mut self, frame: &[u8]) -> Result<()> {
        self.send
            .write_all(frame)
            .await
            .context("发送 control 原始帧失败")
    }

    /// 半关闭发送方向（`FIN`）。
    pub fn finish(&mut self) -> Result<()> {
        self.send.finish().context("关闭 control 发送方向失败")
    }

    /// 对端连接句柄（用于主动关闭连接）。
    pub fn connection(&self) -> &Connection {
        &self.peer
    }
}

impl CtrlReceiver {
    /// 包装一条 quinn 接收流。
    pub fn new(recv: RecvStream) -> Self {
        Self { recv }
    }

    /// 读取下一个 `Envelope`；流正常结束（FIN）返回 `Ok(None)`。
    pub async fn recv_ctrl(&mut self) -> Result<Option<Envelope>> {
        let mut header = [0u8; dfy_proto::FRAME_LEN_PREFIX];
        if !self.read_exact_or_eof(&mut header).await? {
            return Ok(None);
        }
        let declared = u32::from_be_bytes(header) as usize;
        anyhow::ensure!(
            declared <= MAX_FRAME_LEN,
            "对端声明的 control 帧长度 {declared} 字节超过上限 {MAX_FRAME_LEN} 字节"
        );
        let mut body = vec![0u8; declared];
        if !self.read_exact_or_eof(&mut body).await? {
            anyhow::bail!("control 流在帧体读到一半时结束（声明 {declared} 字节）");
        }
        let envelope = decode_frame_body(&header, &body)?;
        trace!(
            kind = envelope.kind_name(),
            bytes = declared,
            "← control 帧"
        );
        Ok(Some(envelope))
    }

    /// 读满 `buf`；流在读第一个字节前就 `FIN` 时返回 `Ok(false)`。
    async fn read_exact_or_eof(&mut self, buf: &mut [u8]) -> Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.recv.read(&mut buf[filled..]).await {
                Ok(Some(0)) | Ok(None) => {
                    if filled == 0 {
                        return Ok(false);
                    }
                    anyhow::bail!(
                        "control 流提前结束：期望 {} 字节，只读到 {filled} 字节",
                        buf.len()
                    );
                }
                Ok(Some(n)) => filled += n,
                Err(e) => return Err(anyhow::Error::new(e).context("读取 control 流失败")),
            }
        }
        Ok(true)
    }
}

/// 复用 `dfy_proto::decode_frame` 的校验逻辑（输入已含长度前缀）。
fn decode_frame_body(header: &[u8; 4], body: &[u8]) -> Result<Envelope> {
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header);
    frame.extend_from_slice(body);
    decode_frame(&frame).context("解析 control 帧失败")
}

impl CtrlChannel {
    /// 由一条双向流的收发两端构造。
    pub fn new(send: SendStream, recv: RecvStream, peer: Connection) -> Self {
        Self {
            tx: CtrlSender::new(send, peer),
            rx: CtrlReceiver::new(recv),
        }
    }

    /// 发送一个 `Envelope`。
    pub async fn send_ctrl(&mut self, envelope: &Envelope) -> Result<()> {
        self.tx.send_ctrl(envelope).await
    }

    /// 读取下一个 `Envelope`（`None` = 对端正常关闭）。
    pub async fn recv_ctrl(&mut self) -> Result<Option<Envelope>> {
        self.rx.recv_ctrl().await
    }

    /// 「发一条、收一条」：用于请求/应答式握手步骤。
    pub async fn exchange(&mut self, envelope: &Envelope) -> Result<Envelope> {
        self.send_ctrl(envelope).await?;
        self.recv_ctrl()
            .await?
            .with_context(|| format!("对端在应答 {} 之前关闭了 control 流", envelope.kind_name()))
    }

    /// 拆成独立的发送/接收句柄（`tokio::select!` 中可同时借用）。
    pub fn split(self) -> (CtrlSender, CtrlReceiver) {
        (self.tx, self.rx)
    }

    /// 对端连接句柄。
    pub fn connection(&self) -> &Connection {
        self.tx.connection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfy_proto::envelope;

    #[test]
    fn frame_body_decode_matches_proto() {
        let env = Envelope::ping(1234);
        let frame = encode_frame(&env).expect("编码");
        let (header, body) = frame.split_at(dfy_proto::FRAME_LEN_PREFIX);
        let mut header_arr = [0u8; 4];
        header_arr.copy_from_slice(header);
        let decoded = decode_frame_body(&header_arr, body).expect("解码");
        assert_eq!(decoded, env);
        assert!(matches!(decoded.body, Some(envelope::Body::Ping(_))));
    }
}
