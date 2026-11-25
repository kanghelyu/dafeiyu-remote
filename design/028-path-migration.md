# 028 P2P ↔ 中继路径迁移（Path Migration v0.1）

## 1. 为什么需要
业主决策（notes/000-owner-brief.md §17）：**P2P 优先 → 质量检测（延迟/丢包/带宽/稳定性）→ 质量合格保持 P2P → 不合格或失败时中继接管**；
且 `P2P ↔ Relay` 切换**必须是可恢复的会话迁移，而不是断开重连**。

027 已解决「迁移后如何续帧」：客户端在新连接上手握完毕后发 `SessionResume(last_frame_id, last_frame_seq)`，
agent 调 `capture.request_keyframe()` 并回 `SessionResumeOk`，采集/编码线程常驻（`stay_alive: true`），
因此迁移只会丢若干帧、等一个关键帧，不会重建编码器、不会让用户看到"断流重连"。

本设计补上 027 §4.2/4.3 缺的那一层：**什么时机切、切到哪、切失败/切错时如何回滚**，
并把它实现为纯逻辑 crate `crates/dfy-path`（无 IO、无时钟、无 tokio），便于确定性单测。

## 2. 分层与职责
```
dfy-transport / dfy-session        dfy-path（本 crate）              dfy-cli
┌────────────────────┐  采样值   ┌──────────────────────┐  Action  ┌──────────────────┐
│ Heartbeat::rtt_ms  │ ────────▶ │ PathSample 滑动窗口   │ ───────▶ │ 建新端点/握手     │
│ ClientRunStats     │           │ EWMA/丢包/速率/stall  │          │ + SessionResume   │
│ 帧时间戳（stall）  │           │ 状态机 + 阈值 + 事件  │ ◀─────── │ on_switch_committed│
└────────────────────┘           └──────────────────────┘  回填结果  └──────────────────┘
```
- `dfy-path` 只依赖 `std`：不读系统时钟（调用方传 `at_ms`）、不建连接、不打日志（调用方消费 `PathEvent`）。
- 具体指标类型不由本 crate 引用（避免与 `dfy-session`/`dfy-transport` 循环依赖），调用方负责把 `rtt_ms`/丢包/速率/stall 折算成 `PathSample`。

## 3. 采样与窗口（`src/sample.rs`）
```rust
PathSample { rtt_ms: f32, loss_ppm: u32, recv_bps: u64, stall_ms: u64, at_ms: u64 }
```
- 默认每 250ms 一条采样（`rtt_ms` 取 `Heartbeat.rtt_ms()`，无心跳时置 `NaN`；`loss_ppm` 由收发包计数差折算；`stall_ms = now - last_frame_at`）。
- `SampleWindow`：环形缓冲（默认容量 32 ≈ 8s），维护 **EWMA α=0.2** 的平滑 RTT、窗口平均丢包、平均接收速率、最新 stall。
- 非法输入先 `clamped()` 清洗：负/非有限 RTT → `NaN`（视为"无 RTT 样本"，不参与 EWMA），RTT 上限 60s，`loss_ppm` 上限 1e6；**任何输入都不 panic**。

## 4. 状态机（`src/state.rs` / `src/machine.rs`）
```text
                 ┌─────────────────────────── Stable(Direct) ───────────────────────────┐
                 │ 3 个坏采样（间隔 ≥500ms）：rtt>2×基线 或 loss>2% 或 stall>2s            │
                 │ → Deteriorated{Direct} + SwitchRequested{Direct→Relay}                │
                 ▼                                                                       │
        Switching{ToRelay, started_at} ──commit(耗时≤switch_timeout)──▶ Observing{since}
                 │                                                            │  5s 无劣化
                 │ 超时/握手失败                                               ▼
                 │ → RolledBack{Direct}                                    Stable(Relay)
                 ▼                                                            │
            Backoff{until=at+5s} ◀── 观察期内 1.5s 内 3 个坏采样 ── Observing ─┤
                 │                    → RolledBack{原路径}                    │ Direct 采样过期(>2s)
                 │ 5s 到期                                                   ▼
                 └──────────────────────────────▶ Stable ───────── Probing{since}
                                                                              │ 连续 5 个好采样
                                                                              │ 且距上次切换 ≥30s
                                                                              │ → Recovered + SwitchRequested{Relay→Direct}
                                                                              ▼
                                                                     Switching{ToDirect} …
```
- `Phase { Stable, Probing{since_ms}, Switching{direction, started_at_ms}, Observing{since_ms}, Backoff{until_ms} }`；
  `active: PathKind { Direct, Relay }`、`pending: Option<PendingSwitch>`、`baseline: Option<Baseline>`。
- 纯函数接口：`on_sample(sample) -> Action`（喂当前路径）、`on_sample_for(path, sample)`（双路径句柄时区分）、
  `on_switch_committed(at_ms, took_ms) -> bool`、`on_switch_failed(at_ms) -> bool`；
  `Action { None, SwitchTo(PathKind), RequestProbe(PathKind) }`。
- 事件（`drain_events()` 取走，供 CLI 打点）：`Deteriorated{path, reason}`、`SwitchRequested{from, to}`、
  `SwitchCommitted{to, took_ms}`、`RolledBack{to}`、`Recovered`。
- 统计：`PathStats { samples, switches, rollbacks, last_switch_at_ms, current }`。

### 4.1 判定细则
- **基线**：启动后先收集 5 条健康采样（有 RTT 且 `stall ≤500ms`），取 RTT 均值/丢包均值/max stall 作为 `Baseline`；
  基线建立前只做丢包/stall 判定（无基线时 RTT 判据不可用，避免冷启动误判）。
- **P2P 劣化**：3 条坏采样且相互间隔 ≥500ms（≈1.5s 持续性，过滤单次抖动）。坏采样定义（按优先级）：
  1. `stall_ms > 2s` 且基线本身未 stall（"无进展"是最强的劣化信号）；
  2. `loss_ppm > 2%`；
  3. 有基线时 `rtt > 2×max(基线 RTT, 5ms)`。
- **relay 活跃时**：不因 RTT 判 relay 劣化（中继天然多一跳），只看 `stall>2s` 或 `loss>2%`，仅发事件、不切换。
- **切回直连**：relay 活跃且 Direct 采样过期（>2s 没探测）时按 1s 限频返回 `RequestProbe(Direct)`（调用方用保留的直连句柄发心跳探测）；
  探测采样需：有 RTT、丢包 ≤0.5%、stall ≤2s、`rtt ≤ 1.2×max(基线,5ms)`（无基线时 ≤120ms），连续 5 条，
  且距上次切换 ≥30s（冷期，防来回抖）。
- **观察期**：提交切换后 5s 为 Observing；期间新路径出现「1.5s 内 3 个坏采样」→ 立即回滚到上一路径并进入 5s 退避。
- **切换超时**：`Switching` 超过 5s 未 `on_switch_committed` 视为失败（握手/Resume 超时），回滚 + 退避。

## 5. 阈值与调参理由（`PathConfig::default()`，全部可配、有 `sanitized()` 兜底）
| 字段 | 默认值 | 理由 |
|---|---|---|
| `window_capacity` | 32 | ≈8s 窗口：足够看趋势，又不至于对劣化反应过慢 |
| `ewma_alpha` | 0.2 | 时间常数 ≈5 条采样（≈1.25s），平滑抖动但不掩盖持续上升 |
| `baseline_samples` | 5 | ≈1.25s 内建立基线；过滤启动期首包抖动 |
| `baseline_max_stall_ms` | 500 | stall 中的采样不代表链路质量，不纳入基线 |
| `baseline_rtt_floor_ms` | 5.0 | 局域网基线可能 <5ms，`2×基线` 会退化成极敏感阈值，用下限兜底 |
| `degrade_bad_samples` / `degrade_min_spacing_ms` | 3 / 500 | 3 条、≈1.5s 持续：抗瞬时脉冲，又不至于等太久（视频 2s 无帧已可感知） |
| `degrade_rtt_factor` | 2.0 | RTT 翻倍在交互式远程桌面已明显可感（20ms→40ms） |
| `degrade_loss_ppm` | 20_000（2%） | QUIC 有 FEC/重传，>2% 时视频开始花屏/等待关键帧 |
| `degrade_stall_ms` | 2_000 | 2s 无新帧 = 用户已明显卡住，比任何均值指标更直接 |
| `recover_good_samples` | 5 | 反向判定更保守（切换代价是一次重握手 + 关键帧） |
| `recover_rtt_factor` / `recover_loss_ppm` | 1.2 / 5_000（0.5%） | 只有接近基线才值得切回 P2P |
| `recover_abs_rtt_ms` | 120 | 基线尚未建立时的绝对健康线 |
| `min_switch_interval_ms` | 30_000 | 冷期：避免两条路径都临界时来回切换（每次切换都丢帧） |
| `observe_window_ms` | 5_000 | 观察期：覆盖一次握手 + ResumeOk（≤5s）+ 首批帧 |
| `rollback_bad_samples` / `rollback_span_ms` | 3 / 1_500 | 新路径"立即劣化"（<1.5s 内 3 条坏采样）说明切换本身有问题 |
| `backoff_ms` | 5_000 | 回滚后静默 5s，避免同一错误路径反复尝试 |
| `probe_interval_ms` / `probe_stale_ms` | 1_000 / 2_000 | 探测限频 1Hz；2s 没新采样就主动探一次 |
| `switch_timeout_ms` | 5_000 | 与 `run_client` 等 `SessionResumeOk` 的 5s 上限对齐 |

## 6. 与 CLI 的集成点（下一步接线，本 crate 不含 IO）
现状（`crates/dfy-cli/src/real.rs`）：`AgentOptions`/`ViewOptions` 已有 `relay: Option<RelayOptions>` 与 `p2p: Option<P2POptions>`，
但 `if relay { … } else if p2p { … }` 只建**一条**路径，没有决策层。

接线方案：
1. **双句柄**：agent 与 view 各持有 `PathEndpoints { direct: Option<…>, relay: Option<…> }`；两条路径的 QUIC 会话语义完全一致，可各建一个 `Endpoint`/连接。
2. **喂采样**：每 250ms 组装 `PathSample`（`Heartbeat::rtt_ms()`、收发包差算丢包、`now-last_frame_at` 算 stall），
   `match monitor.on_sample(sample)`：
   - `None`：继续；
   - `RequestProbe(Direct)`：在保留的直连连接上发一次心跳/小包，把 RTT 作为 `on_sample_for(Direct, …)` 喂回；
   - `SwitchTo(target)`：在新路径上 bind + QUIC 握手，客户端在新连接里发 `SessionResume(last_frame_id, last_frame_seq)`，
     **最多等 `SessionResumeOk` 5s**；成功则 `on_switch_committed(now, took_ms)`，失败/超时则 `on_switch_failed(now)`。
3. **agent 侧**：采集/编码线程常驻（027），切换时 agent 需要**同时 accept 两条连接**（027 §4.1 的并发迁移），
   新连接收到 Resume 后请求关键帧并接管发送；旧连接保留 **2s 双发**（客户端按 `frame_id` 去重）再关闭——兼顾"最后一帧不丢"与内存占用；
   v0.2 可以先简化为"新连接就绪即关旧连接"，代价是可能多丢几帧。
4. **打点**：消费 `PathEvent` 写 tracing/指标；`PathStats` 周期性上报（switches/rollbacks 是调参依据）。

## 7. 失败与回滚
- 握手失败、`SessionResumeOk` 超时（>5s）、新路径立即劣化 → 统一回滚到旧路径并退避 5s；旧连接在回滚时**尚未关闭**，可直接继续收帧。
- 两条路径都劣化（P2P 坏 + relay 坏）：本状态机停留在当前路径并持续发 `Deteriorated` 事件，由 CLI 决定降码率/提示用户，
  不自动做"第三条路"选择（中继池/多服务器健康检测属后续工作）。
- 时间回退（`at_ms` 倒退）：所有比较用 `saturating_sub`，不会 panic；等价于"条件未满足"。

## 8. 待做（v0.2+）
1. **同 socket 双路径 / 多路径 QUIC**：当前切换是"新 socket + 新连接"，握手会带来额外 RTT；QUIC multipath / connection migration 可做到零握手。
2. **并发迁移**：agent 同时服务新旧两条连接 + 会话交接（027 §4.1），旧连接 2s 双发。
3. **0-RTT 恢复**：TLS 会话票据复用，把迁移的恢复时间压到一个 RTT 内。
4. **中继池**：多中继服务器健康检测、优先级、自动切换（业主 §17 的服务器池部分）。
5. **阈值现场调参**：拿真实弱网数据回归 `degrade_*`/`recover_*`；考虑按带宽/分辨率自适应。
6. **迁移时的剪贴板/文件续传**：进行中的文件块按 `FileAck.received` 续传（027 §4.4）。

## 9. 验收标准
- `cargo test -p dfy-path --all-targets` 全绿（38 个单测 + 5 个集成测试，覆盖：平稳不切换、3 采样劣化切换、观察期回滚+退避、
  relay 健康 + 冷期后切回、切换超时/失败、EWMA/环形窗口、非法输入不 panic、事件文案）；
- `cargo clippy -p dfy-path --all-targets -- -D warnings` 与 `cargo fmt -p dfy-path -- --check` 干净；
- 纯逻辑：无 `unsafe`、无 IO、无系统时钟、无 tokio/网络依赖（`Cargo.toml` 零第三方依赖）；
- CLI 接线后：弱网（RTT 翻倍/2% 丢包/2s 无帧）自动切中继且用户可见断流 ≤1 个关键帧间隔；
  切换后 30s 内不来回切；agent 侧采集线程不重建（`resumes` 递增、`stay_alive` 生效）。