# 模块 API 契约（内部设计，v0.1）

目的：让各 crate 可并行开发、互不阻塞。**接口签名是契约**：实现方可加内部细节，但对外签名/语义变更需同步本文件。

约定：异步统一 `tokio`；错误统一 `anyhow::Result`；跨线程传递用 `tokio::sync::{mpsc, watch, broadcast}`；所有 crate 不得 panic 于正常错误路径。

## 1. dfy-proto（已由 builder 实现）
```rust
pub enum Channel { Control=0, InputEvents=2, Clipboard=4, FileMeta=6, Stats=8 }
pub const DATAGRAM_INPUT_MOVE: u8 = 0;
pub const DATAGRAM_VIDEO: u8 = 1;
pub const DATAGRAM_AUDIO: u8 = 2;
pub fn encode_frame(env: &Envelope) -> Vec<u8>;           // 4B BE len + protobuf
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Envelope, usize)>>;
pub struct VideoHeader { pub flags: u8, pub frame_id: u32, pub frame_seq: u16,
                         pub pkt_idx: u16, pub pkt_cnt: u16, pub ts_us: u32 }
pub fn encode_video_datagram(h: &VideoHeader, payload: &[u8]) -> Vec<u8>;
pub fn decode_video_datagram(d: &[u8]) -> Result<(VideoHeader, &[u8])>;
```

## 2. dfy-capture（Windows DXGI；mac/Linux 后续）
```rust
pub struct MonitorInfo { pub id: String, pub name: String, pub x: i32, pub y: i32,
                         pub width: u32, pub height: u32, pub dpi: u32, pub primary: bool }
pub enum PixelFormat { Bgra8, Rgba8 }
pub struct CapturedFrame { pub width: u32, pub height: u32, pub stride: u32,
                           pub format: PixelFormat, pub data: Vec<u8>,
                           pub timestamp_us: u64, pub dirty: Vec<Rect> }
pub trait ScreenCapturer: Send {
    fn monitors(&self) -> &[MonitorInfo];
    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>>;
    fn recreate(&mut self) -> anyhow::Result<()>;   // ACCESS_LOST / 分辨率切换后
}
pub fn create_capturer(monitor_id: Option<&str>) -> anyhow::Result<Box<dyn ScreenCapturer>>;
```
- 采集端**不负责缩放/像素格式转换**；转换交给 codec 或预处理模块。
- 帧间隔语义：`None` = 超时；`Some` = 新帧（可能为重复帧，编码端可跳过）。

## 3. dfy-codec（编码/解码工厂）
```rust
pub struct EncoderConfig { pub codec: Codec, pub width: u32, pub height: u32, pub fps: u32,
                           pub bitrate_kbps: u32, pub keyframe_interval: u32,
                           pub prefer_hardware: bool }
pub enum Codec { H264, H265 }
pub enum PixelFormatIn { Bgra8, Rgba8, Nv12 }
pub struct EncodedFrame { pub data: Vec<u8>, pub is_keyframe: bool, pub pts_us: u64,
                          pub extra: CodecExtra }        // extra: SPS/PPS、AVCC 等
pub trait VideoEncoder: Send {
    fn encode(&mut self, raw: &[u8], stride: u32, pts_us: u64) -> anyhow::Result<Vec<EncodedFrame>>;
    fn request_keyframe(&mut self);
    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()>;
}
pub fn create_encoder(cfg: &EncoderConfig) -> anyhow::Result<Box<dyn VideoEncoder>>;

pub trait VideoDecoder: Send {
    fn decode(&mut self, data: &[u8]) -> anyhow::Result<Vec<DecodedFrame>>; // DecodedFrame{data,stride,width,height,pts_us}
}
pub fn create_decoder(codec: Codec, prefer_hardware: bool) -> anyhow::Result<Box<dyn VideoDecoder>>;
pub fn available_encoders() -> Vec<EncoderInfo>;  // 探测硬/软编
```
- 后端优先顺序：NVENC → AMF → QSV/oneVPL → VideoToolbox → OpenH264（软编保底）。
- 输出格式：H.264 优先 Annex-B（低延迟、SPS/PPS 随关键帧）；解码器需接受 Annex-B。

## 4. dfy-input（采集端 + 注入端）
```rust
pub struct InputEvent { pub kind: InputKind, pub ts_us: u64 }
pub enum InputKind {
    Key { usage: u16, mods: u8, down: bool, unicode: Option<char> },
    MouseMove { x: i32, y: i32, absolute: bool },      // absolute: 0..65535 归一化输入
    MouseButton { button: u8, down: bool },
    Wheel { delta: i32, horizontal: bool },
    Text { text: String },                             // IME 提交
}
pub trait InputSink: Send { fn apply(&mut self, ev: &InputEvent) -> anyhow::Result<()>; }
pub fn create_injector() -> anyhow::Result<Box<dyn InputSink>>;   // Win: SendInput; mac: CGEvent; Linux: XTest/uinput
pub trait InputSource: Send { fn poll(&mut self) -> Vec<InputEvent>; }  // 预留（虚拟屏/游戏原始输入）
pub enum ShortcutDecision { LocalOs, LocalFunction, Forward, Drop }
pub fn classify_shortcut(ev: &InputKind, local_bindings: &ShortcutMap, forward_system: bool) -> ShortcutDecision;
```
- 键位编码统一 HID Usage（页 0x07 键盘 / 0x0C 消费类）；平台映射表在实现内。
- 注入端必须实现**防卡键**：会话断开/结束时释放所有按下键与鼠标键。

## 5. dfy-clipboard
```rust
pub struct ClipFormat { pub id: u32, pub mime: String, pub hash: u64, pub size: Option<u64> }
pub struct ClipSnapshot { pub formats: Vec<ClipFormat>, pub seq: u64 }
pub trait ClipboardBackend: Send {
    fn snapshot(&mut self) -> anyhow::Result<ClipSnapshot>;
    fn read_format(&mut self, id: u32, offset: u64, max: usize) -> anyhow::Result<Vec<u8>>;
    fn write_formats(&mut self, items: &[(String, Vec<u8>)]) -> anyhow::Result<()>;
}
pub fn create_backend() -> anyhow::Result<Box<dyn ClipboardBackend>>;
```
- 同步策略：变更通知 → 发 FormatList → 对端按需 Pull（块 128KiB，<=4 块未确认窗口）；写回后回传 Applied(hash) 抑制回环。
- MVP 支持 `text/plain;charset=utf-8` 与 `image/png`。

## 6. dfy-transport（builder 实现，供 session 使用）
```rust
pub struct AgentEndpoint { /* listen, accept, handshake */ }
pub async fn agent_listen(addr:SocketAddr, tls:AgentTls, password_hash:[u8;32]) -> Result<AgentEndpoint>;
pub async fn client_connect(addr:SocketAddr, tls:ClientTls, password:&str) -> Result<ClientSession>;
pub struct ClientSession { pub welcome: Welcome, pub fingerprint: [u8;32] }
impl ClientSession {
    pub async fn send_ctrl(&mut self, env: Envelope) -> Result<()>;
    pub async fn recv_ctrl(&mut self) -> Result<Envelope>;
    pub fn send_datagram(&self, data:&[u8]) -> Result<()>;
    pub async fn recv_datagram(&self) -> Result<Vec<u8>>;
}
```
- DATAGRAM 上限 1200B（MTU 安全）；控制流消息上限 8MiB。
- 心跳 5s/超时 15s；`Bye` 需优雅关闭。

## 7. dfy-session（编排层，lead 或集成 agent 实现）
职责：把上面模块拼成"会话"。
```rust
pub struct AgentRuntime { /* capture, encoder, input, clipboard, transport */ }
pub struct ClientRuntime { /* transport, decoder, viewer, input capture, clipboard */ }
```
- Agent 主循环：capture → (dirty 判定/跳过重复) → encoder → 分片(≤1100B) → send_datagram；接收 input-events → injector。
- Client 主循环：recv_datagram → 重组 + FEC → decoder → viewer；输入采集 → send。
- 背压：编码队列最大 2 帧；发送队列溢出丢弃最旧非关键帧并 `request_keyframe`。

## 8. UI（apps/ 或 examples/）
- MVP 用 `winit + softbuffer`（CPU 贴图）先跑通；M1.5 换 wgpu/外部纹理（并给 Flutter 复用）。
- 只依赖 `dfy-session` 的 client 侧 API：`ClientRuntime::start(config) -> FrameSink`。

## 9. 命名/许可证纪律
- 禁止从逆向目标抄：消息名 `gv_pb.*`、常量 `FF_*`/`FT_*`/`CB_*`、bridge 方法名、默认键位表、图标资源、域名/路由。
- 允许借鉴：能力矩阵、快捷键四态、剪贴板清单→拉块、文件 ask→confirm→block、Qt/Web 分层思想（用 Flutter/Web 重写）。
- 每个 crate 头部加 3 行 SPDX：`// SPDX-License-Identifier: AGPL-3.0-or-later` + 版权行。