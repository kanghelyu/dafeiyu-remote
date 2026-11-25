# 真实链路验证记录（M0/M1 里程碑）

> 日期：2026-11-25 ｜ 环境：Windows 11（AMD64），Rust 1.100.0（`stable-x86_64-pc-windows-gnu`），本机显示器 `\\.\DISPLAY9` 2800×1840 @DPI 192。

## 验证目标

不是"协议 demo"，而是:**真实屏幕采集 → Media Foundation H.264 硬件编码 → QUIC DATAGRAM → 分片重组 → Media Foundation H.264 解码 → BGRA 帧**整条链路在真实网络栈上跑通。

## 命令

```powershell
# 被控端（采集 + 编码 + 推流）
cargo run -q -p dfy-cli -- agent --port 21116 --fps 30 --bitrate 4000

# 主控端（连接 + 解码 + 统计 + 保存首帧）
cargo run -q -p dfy-cli -- view --seconds 6 --save-first .tmp/first-frame.bgra
```

## 结果（6 秒）

| 指标 | 数值 |
| --- | --- |
| 视频 DATAGRAM | 398（401.57 KiB） |
| 重组完整帧 | 60 |
| 成功解码帧 | 24 |
| 关键帧 | 1（每 2 秒一个 IDR） |
| 解析失败 | 0 |
| 心跳 RTT | 0.571 ms |
| 首帧 | 2800×1840 BGRA，20,608,000 字节 |

首帧解码结果转 PNG 后肉眼确认是真实桌面画面（DeepSeek Harness 窗口内容清晰可辨），不是伪造纹理。

## 这次验证确认了哪些设计决策

1. **QUIC + DATAGRAM 承载视频可行**：单帧 2800×1840 的 H.264 码流被切成多个 ≤1200 字节 DATAGRAM，`FrameAssembler` 按 `frame_id` 重组，6 秒内 0 解析失败。
2. **MFT 编码器可用**：H.264 MFT 在 QSV/AMF/NVENC/OpenH264 之间自动选择，输入 BGRA8 直接喂 MFT（配置 `MFVideoFormat_NV12` + 输入转换），无需自己写 NV12 转换（转换在 MFT 内部完成）。
3. **解码输出是 NV12**：`dfy-cli/src/adapters.rs` 里做了 `nv12_to_bgra`（BT.709 有限范围）标量实现；后续可换 SIMD/GPU 上屏。
4. **会话层抽象成立**：`dfy-session` 的 `Capturer/Encoder/Decoder` trait 让"真实采集+编码"与"假采集+假编码"共用同一套 `serve_agent`/`run_client` 编排，e2e 测试不需要真实硬件。

## 已知限制 / 下一步

- **解码帧数少于重组帧数（24/60）**：MFT 解码器有内部重排序缓冲，`view` 退出前没有 flush，尾部帧未取出。产品化时需要在会话结束时 `flush()` 并等待。
- 采集只在桌面有变化时出帧（DXGI Desktop Duplication 语义），静态桌面帧率会掉到 0；真实产品要配合光标位置/定时关键帧策略。
- 还没有上屏 UI（当前只统计 + 落盘首帧），下一步接 winit/Flutter 外部纹理。
- 输入注入（`dfy-input`）与剪贴板（`dfy-clipboard`）已实现但还没串进 `agent/view` 会话循环，下一步在 `CtrlHandler` 里接 `InputSink` 与 `ClipboardBackend`。
- 中继（`dfy-relay`）已能独立跑通，但客户端还没接 `relay.addr` 字段；下一步在 `dfy view` 增加 `--relay` 模式。