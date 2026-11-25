# crates/ — 大肥鱼远程 (DFYP/1) Rust 工作区

一条 QUIC 连接承载控制帧、视频/输入 DATAGRAM；自研线格式，无第三方协议字段号。

| crate | 职责 |
| --- | --- |
| `dfy-proto` | 线格式唯一来源：手写 prost 消息（`Envelope` oneof）、4B 大端长度帧、视频 DATAGRAM 头、input-move 编解码、通道/权限/错误码常量。无 protoc 依赖。 |
| `dfy-transport` | quinn+rustls+rcgen 封装：ALPN `dfyp/1`、rcgen 自签身份 + SPKI-SHA256 指纹（可选严格校验）、`Hello→Challenge→Auth→Welcome` 握手（MVP: HMAC-SHA256(SHA256(pw), nonce)）、control 流 `send_ctrl/recv_ctrl`、5s/15s 心跳、视频分片发送与接收统计。 |
| `dfy-cli` | `dfy` 命令：`genkey` / `demo-agent` / `demo-client` / `selftest`，demo 与诊断入口。 |
| `dfy-capture` | Windows DXGI 桌面采集（另一位 owner 维护，本 crate 不依赖它做 demo）。 |

## 自测

```powershell
# 先设 PATH（cargo + mingw gcc），再：
cargo build --workspace
cargo test --workspace
cargo run -p dfy-cli -- selftest        # 期望最后一行 SELFTEST OK，exit 0

# 手动对跑（两个终端）
cargo run -p dfy-cli -- demo-agent  --port 21116 --password test
cargo run -p dfy-cli -- demo-client --addr 127.0.0.1:21116 --password test \
    --expect-fingerprint <demo-agent 打印的 64 位 hex 指纹>
```

MVP 遗留：口令派生换 Argon2id、证书指纹固定/TOFU 持久化、DATAGRAM FEC 与关键帧重传、
真实采集/编码链路、完整 session 状态机与权限执行。