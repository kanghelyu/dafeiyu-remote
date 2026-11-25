# 027 会话恢复与路径迁移（Session Resume v0.1）

## 1. 为什么需要
业主决策（notes/000-owner-brief.md §17）：P2P 优先，质量不合格时切中继，**切换必须是可恢复的会话迁移，而不是断开重连**。
网络路径变化（Wi-Fi 切换、NAT 重绑定、P2P 打洞失败）时，采集器、编码器状态在 agent 侧是连续的；
只要客户端能声明"我上次看到第几帧"，agent 请求一个关键帧，视频就能从断点续上。

## 2. v0.1 已实现
### 2.1 协议消息（`crates/dfy-proto/src/msg.rs`，tag 22/23）
- `SessionResume { last_frame_id: u64, last_frame_seq: u64 }`：客户端 → agent，声明已看到的视频进度（0 = 还没收到帧）。
- `SessionResumeOk { keyframe_requested: bool, resume_from_frame_id: u64 }`：agent 确认，并给出下一帧的 frame_id。

### 2.2 agent 侧（`crates/dfy-session/src/agent.rs`）
- `serve_agent` 收到 `SessionResume`：`capture.request_keyframe()`（采集线程仍在运行），回 `SessionResumeOk`，
  `AgentRunStats.resumes` 计数；不结束会话、不重建采集。
- `AgentConfig.stay_alive = true` 时会话结束保留采集线程（`real.rs run_agent` 已默认启用），
  因此断线/切换后的下一次连接直接复用同一个 `CaptureLoop`。

### 2.3 client 侧（`crates/dfy-session/src/client.rs`）
- `run_client` 建立后立即发 `SessionResume(0,0)`，**等待 `SessionResumeOk`（最多 5s）再进入收帧循环**
  （避免帧洪峰先到、客户端收满帧退出而错过确认）；`ClientRunStats.resume_ok/last_frame_id` 记录结果。

## 3. 验证
- `crates/dfy-session/tests/e2e.rs` 新增 `resume_after_reconnect_continues_frames`：
  agent 用 `stay_alive: true` 顺序 accept 两次连接，client 两次 `run_client`；
  断言二次会话 `resume_ok == true`、继续交付帧、agent 侧 `resumes == 2`。
- 全 workspace 质量门（fmt/clippy -D warnings/test）绿。

## 4. 尚未实现（v0.2+）
1. **并发切换**：当前 `serve_agent` 一次只服务一个连接，第二个连接要等第一个结束；
   真正无缝迁移需要 agent 侧同时 accept 两条连接 + 会话交接（新连接接管发送、旧连接 2s 双发后关闭）。
2. **质量检测与策略**：延迟/丢包/带宽采样、劣化判定（3 采样/1.5s）、P2P↔relay 自动切换与回滚。
3. **客户端路径管理**：同时维护 P2P/relay 两条候选路径（prepared path），切换时 0-RTT 恢复 TLS 会话。
4. **剪贴板/文件状态续接**：迁移时传输中的文件块需从 `FileAck.received` 续传。
