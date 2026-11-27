# 音频链路第一步（2026-11-27）

## 范围

- `dfy-proto`：新增音频 DATAGRAM 格式 `chan(2) | u32 seq | u32 ts_us | s16le PCM`，上限 1100B；常量固定 16 kHz / 单声道 / 20 ms（320 sample = 640 B）。`encode_audio`/`decode_audio` 与 `AudioPacket` 有单测，覆盖往返、非法长度、短头、错通道。
- 新 crate `dfy-audio`：cpal 0.18 设备后端放在可选 feature `device-backend` 后；默认只编译 `NullSource`/`NullSink` 降级，因此 Linux 缺 `libasound2-dev` 也能构建。`AudioSource`/`AudioSink` trait、`DeviceSource`/`DeviceSink`（Windows WASAPI、Linux ALSA/Pulse、macOS CoreAudio）、`PcmFrame::to_datagram`/`from_datagram` 完整。
- 采集/播放回调只做队列操作，不在设备线程阻塞；`next_frame` 按 20ms 窗口取一帧，取不到返回 `None`。

## 真实验收

- `cargo test -p dfy-audio --features device-backend`：5 通过 / 0 失败（配置与帧长、PCM DATAGRAM 往返、枚举状态、默认输出设备播放、Null 后端）；无 feature 默认构建的 strict clippy 与 Linux guest `cargo check -p dfy-audio` 均通过（`.tmp/audio-linux-check2-20261127.log`）。
- Windows 本机 cpal 枚举实测：`host=WASAPI inputs=0 outputs=1 default_input=false default_output=true`（`.tmp/dfy-audio-device-enum-20261127.log`）。
- 真实默认输出设备打开并成功播放一帧 640B PCM：`.tmp/dfy-audio-output-device-20261127.log`，测试 `default_output_device_accepts_pcm` 通过。
- `cargo test -p dfy-proto`：22 unit + 1 doctest 通过；strict clippy 通过。

## 2026-11-27 续：接入 agent/view 会话

- `dfy-cli` 新增 `--audio-out`（Agent 采集并发送）与 `--audio-in`（View 接收并播放）；Agent 开启时权限位图加入 `PERM_AUDIO_OUT`。View 收到 `CHAN_AUDIO` DATAGRAM 后解码为 `PcmFrame` 并写入默认输出设备，统计输出“音频 DATAGRAM / 解码帧 / 最后序号”。
- 因本机无输入设备，Agent 增加诊断开关 `--audio-test-tone`（合成 16kHz 正弦波代替麦克风）；明确标注为诊断，不冒充真实采集。
- 跳过音频时行为不变；音频设备不可用时 View 降级为不播放并继续收视频。

### 真实验收（跨 OS）

- Windows Debug `dfy.exe` SHA-256 `28284dbeb0cc7b05437c9e94626827950ccc4ea065460ccadeb9eeae17e15418`；Linux guest dfy 由当日同步源码构建。
- Linux Agent：`DISPLAY=:99 dfy agent --host 0.0.0.0 --port 24800 --audio-out --audio-test-tone --codec mjpeg`；Windows View：`dfy view --addr 127.0.0.1:24800 --audio-in --seconds 10`（QEMU hostfwd 直连 UDP，不是两台物理设备/公网）。
- 结果：`view_exit=0`、`结束原因=duration`；音频 DATAGRAM 446 / 解码帧 446 / 最后序号 445（无丢号）；视频 520 DATAGRAM / 6 解码 / 0 解析失败；RTT 3.552ms。报告 `.tmp/audio-live-crosos-20261127/report.json` SHA-256 `66d851e4fc1a6d932cd517763be26f2cb11d5ce1a13ce567fd7a7a23435006d5`，View 日志 SHA-256 `9a118c822ad3971e04e26629358c8366888697ee1ac7106c0ab85ac058b003d7`。

## 2026-11-27 续：抖动缓冲与迁移期音频

- `AudioJitterBuffer`：按 seq 重排、重复/乱序计数、丢包在送出时按空隙确认（避免迟到包误计），固定延迟吸收抖动；`reset_sequence()` 在 Agent 因迁移重建音频任务（seq 归零）时调用。单测覆盖重排、重复、丢包、reset。
- View 收到音频包先入抖动缓冲，由 20ms tick 平滑写入播放设备；统计增加“丢包/乱序/重复/峰值深度”。迁移时先尽力收干旧连接队列中尚未读取的音频包，再重置序号。
- 目标延迟实测从 20ms → 60ms → 100ms：本机 QEMU 抖动下 20ms 丢 43 帧、60ms 丢 2 帧、100ms 丢 0 帧；最终采用 100ms（延迟/丢包取舍，未做更细标定）。

### 真实验收（强制迁移）

- Windows Debug `dfy.exe` SHA-256 `e4f77a5294c7ee8f011970b0e96ee94e4c2f38f288fc65883120f5f3c98cd57e`。
- Linux Agent（`--audio-out --audio-test-tone`，`--auto-path`）→ Windows View（`--audio-in`，`--auto-path --force-switch-once relay`），QEMU NAT 上本机中继；18s 会话。
- 结果：`migrated=true`、`resume_ok=true`、`帧#12/12`、`view_exit=0`、duration 结束；新纪元音频 572 DATAGRAM / 569 帧写出 / 最后序号 559 / 丢包 0 / 淘汰 0 / 乱序 0 / 重复 0 / 峰值深度 9。报告 `.tmp/audio-migration-20261127/report.json` SHA-256 `19e7fbe23fc358b19193097314083dc6b86df14b4578e06248a57de1e46bf44d`。
- 差额根因已定位并修复：先前把旧连接队列里积压的约 243 个旧纪元音频包塞进缓冲，容量 64 触发 179 次淘汰，随后 `reset_sequence()` 又清空剩余 64 帧，因此出现“814 收 / 571 播、丢包 0”的假象。现在旧纪元包随 Connection 一起丢弃，统计如实反映新纪元。

## 2026-11-27 续：音量/静音与自适应抖动

- `GainSink` 对所有 PCM 样本做线性增益（音量 0..=100，静音输出 0，饱和到 i16）；View 新增 `--volume`/`--mute`，仍接收并解码音频，只在写设备前应用增益。单测验证 100% 恒等、50% 增益、静音清零、越界音量夹取。
- 抖动缓冲新增 `adapt(depth)`：深度 >8 时目标延迟每次 +10ms（上限 200ms），空闲时每次 -2ms（下限 20ms）；单测验证上下限与回收。
- 实测：Windows Debug dfy SHA-256 `937f47581dbe0182b58bdfaa839ad688e3f9c893946b37078c10f1630b40ede7`；跨 OS 10s 直连会话加 `--volume 50 --mute`，日志显示“音频播放已启用（音量 50%，静音）”，音频 448 DATAGRAM / 447 帧写出 / 丢包 0 / 淘汰 0；报告 `.tmp/audio-live-crosos-20261127/report.json` SHA-256 `fd3b41bf1ca3c1e3d2fa559cb036b422ad1851bac36683de43eb94aa6716e138`。
- 边界：静音只验证到“送往设备前样本为 0”的单测，未测量扬声器实际输出；自适应延迟未做长稳标定。

## 边界

- 本机没有输入设备（inputs=0），**真实麦克风采集链路未实测**；验收使用的是诊断正弦波，`DeviceSource` 只有编译与逻辑测试，不记为真实采集通过。
- 迁移后音频继续播放且无确认丢包，但 814 包中只有 571 帧写出；差额来自抖动缓冲的迟到丢弃与序号重置，尚未逐帧归因，不把它记为“零间隙”。
- 没有音量/静音控制、抖动自适应或丢包补偿；100ms 目标延迟是本机实测的取舍，不是通用标定。
- 时间戳采用 u32 微秒截断（约 71 分钟回绕），与视频 DATAGRAM 一致；没有抖动缓冲、丢包补偿、静音检测或回声消除。
- cpal 为新增依赖，已解析并锁定 14 个包；Linux/macOS 只做了源码编译准备，真机未验证。
