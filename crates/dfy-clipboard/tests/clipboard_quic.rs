//! 剪贴板拉模式在真实 QUIC 会话上的端到端测试。
//!
//! 两个 `ClipboardSync` 实例通过 `dfy-transport` 的 control 流交换 tag 13–16 消息：
//! 提供方（client）广播清单 → 消费方（agent）拉取 → 提供方分块回传 → 消费方写回并回 `Applied`。
//! 断言：消费方后端收到写入且内容逐字节一致、提供方收到 `Applied`。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use dfy_clipboard::{
    message_from_envelope, message_to_envelope, ClipFormat, ClipKind, ClipboardBackend,
    ClipboardSync, DataBlock, FormatList, SyncMessage,
};
use dfy_transport::{
    bind_agent_random, client_handshake, client_tls, connect, server_handshake,
    tls::install_crypto_provider, ServerAuth,
};

const TEXT: &str = "大肥鱼剪贴板 · pull model over QUIC";

type WriteLog = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// 测试用内存后端：单格式文本 + 共享写入日志。
struct FakeBackend {
    seq: u64,
    text: Option<String>,
    log: WriteLog,
}

impl FakeBackend {
    fn empty(log: WriteLog) -> Self {
        Self {
            seq: 1,
            text: None,
            log,
        }
    }

    fn with_text(text: &str, log: WriteLog) -> Self {
        Self {
            seq: 1,
            text: Some(text.to_string()),
            log,
        }
    }
}

impl ClipboardBackend for FakeBackend {
    fn sequence(&self) -> Result<u64> {
        Ok(self.seq)
    }

    fn list_formats(&self) -> Result<FormatList> {
        let formats = if self.text.is_some() {
            vec![ClipFormat {
                id: 13,
                name: "CF_UNICODETEXT".to_string(),
                kind: ClipKind::Text,
            }]
        } else {
            Vec::new()
        };
        Ok(FormatList {
            sequence: self.seq,
            formats,
        })
    }

    fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock> {
        let data = self
            .text
            .as_ref()
            .map(|t| t.as_bytes().to_vec())
            .unwrap_or_default();
        let total_len = data.len() as u64;
        let start = (offset as usize).min(data.len());
        let end = if max == 0 {
            start
        } else {
            (start + max).min(data.len())
        };
        Ok(DataBlock {
            id: format_id,
            total_len,
            offset,
            block: data[start..end].to_vec(),
        })
    }

    fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()> {
        let name = match kind {
            ClipKind::Text => "CF_UNICODETEXT",
            ClipKind::Html => "HTML Format",
            ClipKind::Png => "PNG",
            ClipKind::Dib => "CF_DIB",
            ClipKind::FileList => "CF_HDROP",
            ClipKind::Rtf => "Rich Text Format",
            ClipKind::Other(n) => n.as_str(),
        };
        self.seq += 1;
        assert!(files.is_empty(), "本测试只同步文本");
        self.log
            .lock()
            .expect("写入日志锁中毒")
            .push((name.to_string(), bytes.to_vec()));
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_pull_roundtrip_over_quic() {
    install_crypto_provider();

    let (endpoint, identity, addr) = bind_agent_random().expect("绑定 Agent 端点");
    let fingerprint = identity.fingerprint;
    let agent_log: WriteLog = Arc::default();

    // ---- Agent / 消费方 ----
    let agent = {
        let agent_log = Arc::clone(&agent_log);
        tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("端点已关闭");
            let conn = incoming.await.expect("QUIC 连接建立失败");
            let (welcome, mut chan) = server_handshake(&conn, &ServerAuth::new("pw", "clip-agent"))
                .await
                .expect("Agent 握手失败");
            assert_eq!(welcome.agent_name, "clip-agent");

            let mut sync = ClipboardSync::new(Box::new(FakeBackend::empty(agent_log)));
            let mut applied = false;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

            while tokio::time::Instant::now() < deadline {
                let msg = match tokio::time::timeout_at(deadline, chan.rx.recv_ctrl()).await {
                    Ok(Ok(Some(env))) => env,
                    Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
                };
                let Some(sync_msg) = message_from_envelope(&msg) else {
                    continue;
                };
                if matches!(sync_msg, SyncMessage::Applied { .. }) {
                    applied = true;
                    break;
                }
                let mut sent_applied = false;
                for out in sync.on_remote(&sync_msg).expect("消费方处理失败") {
                    if matches!(out, SyncMessage::Applied { .. }) {
                        sent_applied = true;
                    }
                    chan.tx
                        .send_ctrl(&message_to_envelope(&out).expect("消息可编码"))
                        .await
                        .expect("发送控制帧失败");
                }
                if sent_applied {
                    applied = true;
                    break;
                }
            }

            assert!(applied, "消费方未收到 Applied");
            // 让刚发出的 Applied 帧有机会送达对端再断开连接。
            tokio::time::sleep(Duration::from_millis(300)).await;
        })
    };

    // ---- Client / 提供方 ----
    let (client_endpoint, conn) =
        connect(addr, client_tls(Some(fingerprint)).expect("TLS 配置失败"))
            .await
            .expect("连接 Agent 失败");
    let (_welcome, mut chan) = client_handshake(&conn, "pw", "clip-client", vec![7, 7, 7])
        .await
        .expect("Client 握手失败");

    let provider_log: WriteLog = Arc::default();
    let mut sync = ClipboardSync::new(Box::new(FakeBackend::with_text(
        TEXT,
        Arc::clone(&provider_log),
    )));
    let list = sync
        .poll_local()
        .expect("轮询本地剪贴板失败")
        .expect("应有清单");
    chan.tx
        .send_ctrl(&message_to_envelope(&list).expect("清单可编码"))
        .await
        .expect("发送清单失败");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut applied = false;
    while tokio::time::Instant::now() < deadline {
        let msg = match tokio::time::timeout_at(deadline, chan.rx.recv_ctrl()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        };
        let Some(sync_msg) = message_from_envelope(&msg) else {
            continue;
        };
        if matches!(sync_msg, SyncMessage::Applied { .. }) {
            applied = true;
            break;
        }
        for out in sync.on_remote(&sync_msg).expect("提供方处理失败") {
            chan.tx
                .send_ctrl(&message_to_envelope(&out).expect("消息可编码"))
                .await
                .expect("发送控制帧失败");
        }
    }

    assert!(applied, "提供方未收到 Applied");
    tokio::time::timeout(Duration::from_secs(5), agent)
        .await
        .expect("Agent 任务超时")
        .expect("Agent 任务 panic");

    let writes = agent_log.lock().expect("写入日志锁中毒").clone();
    assert_eq!(writes.len(), 1, "消费方应恰好写入一次");
    assert_eq!(writes[0].0, "CF_UNICODETEXT");
    assert_eq!(
        String::from_utf8(writes[0].1.clone()).expect("写入内容不是 UTF-8"),
        TEXT,
        "剪贴板内容往返一致"
    );
    assert!(
        provider_log.lock().expect("写入日志锁中毒").is_empty(),
        "提供方不应被写回"
    );

    client_endpoint.close(0u32.into(), b"done");
}
