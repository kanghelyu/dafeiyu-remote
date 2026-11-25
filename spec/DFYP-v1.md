# DFYP/1 — 大肥鱼远程 协议规范（v1 草案）

> 本文是 clean-room 设计规范：由产品/架构目标独立推演，**不复制任何第三方协议的字段号、消息名或常量表**（包括 UU远程 `gv_pb`、RustDesk 定义）。
> 规范文本按 Apache-2.0 / CC-BY-4.0 发布以便生态实现；代码实现按 AGPL-3.0-or-later。

## 0. 术语
- **Agent（被控端）**：运行在被远程控制的机器上，负责采集/编码/注入/文件。
- **Client（主控端）**：用户操作的一端，负责解码/渲染/输入采集。
- **Rendezvous（RDV，信令/ID 服务）**：设备注册、在线态、候选交换、中继票据。
- **Relay**：不透明白转发 UDP/TCP 的中继（零知识，只按会话 ID 转发密文）。
- **Session**：一次点对点串流会话，拥有独立密钥与通道集合。
- **Capability Matrix**：双方能力协商结果（编码器/分辨率/功能位）。

## 1. 身份与信任
- 设备密钥：Ed25519（签名）/ X25519（密钥交换）；私钥存 OS keystore（Windows DPAPI、macOS Keychain、Linux libsecret、Android Keystore）。
- `device_id`：9 位数字（对外展示，用户熟悉）；内部 `device_key_id = BLAKE3(pubkey)[:16]`（base32 展示）。
- 设备证书：自签 X.509 v3，SPKI = 设备 Ed25519 公钥；有效期 5 年，到期轮换。
- 会话鉴权：
  1. TLS 1.3 双向认证（rustls 自定义 verifier：对端证书 SPKI 必须等于 RDV 通告的公钥）；
  2. 应用层密码（Argon2id 存储于 Agent，E2E 通道内校验）或一次性验证码（协助模式）；
  3. 权限位（见 §7）。
- 威胁模型：RDV 是身份绑定的信任根；提供 TOFU + 指纹校验 + 企业固定公钥（`trust_policy`）缓解 MITM。Relay 只见密文。

## 2. 传输栈
```
应用层  session / input / clipboard / file / audio / video / telemetry
通道层  QUIC streams (control,input-events,clipboard,file-meta,file-data,stats)
        QUIC DATAGRAM (input-move, video, audio)
传输层  QUIC v1 (quinn, TLS1.3, DATAGRAM RFC9221, connection migration)
回退层  若 UDP 不可用：TLS/TCP:443 多路复用帧（4B len + 1B chan + payload），语义同 QUIC 流
网络层  ICE-lite 候选（host/srflx/relay）+ 自有 relay
信令层  WSS + REST（JWT）；消息 protobuf
```
- 端口：Agent 常驻 UDP 21116（可配）；TLS/TCP 回退复用 443（经 relay 或 Agent 自身 TLS 端点）。
- MTU：QUIC 初始 1200；视频分片负载 ≤ 1100B。
- 加密细节：TLS1.3 内建；应用层不再重复加密（除文件内容可选二次 AEAD，用于跨 relay 存储场景，非 v1 必需）。

## 3. 通道表
| 通道 | 类型 | 可靠 | 内容 |
|---|---|---|---|
| `control` | 双向流 0 | 是 | 握手/能力/显示器/会话控制/心跳/统计订阅 |
| `input-events` | 双向流 2 | 是 | 键、键释放、点击、滚轮、触摸、剪贴板触发、IME 文本 |
| `input-move` | DATAGRAM 0 | 否 | 鼠标绝对/相对位移、手柄轴（可丢旧帧，序号单调） |
| `video` | DATAGRAM 1 | 否+FEC | 帧元数据 + 分片；帧内序号；keyframe 标记 |
| `audio` | DATAGRAM 2 | 否+FEC | Opus 20ms 帧 |
| `clipboard` | 双向流 4（v1 未启用） | 是 | 大对象分块；v1 小对象复用 control 流 tag 13–16 |
| `file-meta` | 双向流 6 | 是 | 元数据、确认、取消、进度 |
| `file-data` | 单向流 6..31 | 是 | 1–4MiB 块 + BLAKE3 |
| `stats` | 单向流 8 | 是 | 1–2Hz：RTT、loss、fps、码率、队列、解码耗时 |

消息统一帧：`varint len | varint type | payload(protobuf)`（control/流通道）；DATAGRAM 独立头部见 §6。

## 4. 握手（control 流）
1. Client→Agent：`Hello{proto_min=1, proto_max=1, client_info{platform,version,name}, device_pubkey}`（若 TLS 已完成，pubkey 必须一致）。
2. Agent→Client：`Challenge{nonce(32B), auth_modes[]}`。
3. Client→Agent：`Auth{ mode: PASSWORD|ASSIST_CODE|PIN, proof = HMAC-SHA256(pake? ) }`——v1 用「密码哈希（Argon2id 输出）与 nonce 的 HMAC」；协助码模式由 RDV 预置一次性 token。
4. Agent→Client：`Welcome{session_id, agent_info, capabilities{ codecs[], max_res, hw_enc, hdr, virtual_display, clipboard_formats, audio, terminal, file, multi_monitor }, permission_bitmap, monitor_list[{id,w,h,dpi,name,primary}]}`。
5. 双方 `CapabilityAck` 后进入会话；失败 `Reject{code}`。
- 心跳：control 上 `Ping/Pong`，5s 间隔；15s 无响应判定断线；断线重连保留会话 <30s（0-RTT 迁移）。
- 分辨率/帧率/码率协商走 `VideoConfig` 双向可变更消息（ABR）。

## 5. 控制消息目录（v1）
- 会话：`Ping`/`Pong`/`SetMonitor`/`VideoConfig`/`EncoderSwitch`/`RequestKeyframe`/`PermissionUpdate`/`PrivacyScreen`/`RemoteMute`/`Bye{reason}`。
- 显示器：`MonitorListUpdate`/`MonitorModeChanged`/`CursorShape`/`CursorImage{pos,hotspot,png}`/`CursorVisible`。
- 系统：`SystemAction{kind: LOCK|REBOOT|SHUTDOWN|SLEEP|WAKE_ON_LAN}`/`TerminalSession{op}`/`ClipboardPolicy`/`FilePolicy`。
- 统计：`Stats{...}`/`NetworkProbe{}`（RTT/带宽探测）。
- 错误码：`E_AUTH`、`E_PERM`、`E_VERSION`、`E_BUSY`、`E_TIMEOUT`、`E_NET`、`E_CLIPBOARD_BUSY`、`E_FILE_DENIED`、`E_ENCODER_UNAVAILABLE`。

## 6. 视频/输入通道
- 视频 DATAGRAM 头：`u8 chan(1) | u8 flags | u32 frame_id | u16 frame_seq | u16 pkt_idx | u16 pkt_cnt | u32 ts_us | payload`；flags: KEY/LAST/FEC/XOR_GROUP。
- 帧级 FEC：每 N 包一组 XOR 包（N 自适应 4–16），RS(10,3) 可选；缺包 → `RequestKeyframe`（PLI）。
- 输入事件模型：`InputEvent{ kind: KEY|MOUSE_BTN|WHEEL|TOUCH|PAD|TEXT, hid_usage, mods, state, x,y(0..65535), dx,dy, unicode?, ts_us }`。
  - key/btn/wheel/touch → input-events（可靠）；mouse move/pad axis → input-move（不可靠）。
  - 统一 HID Usage（键盘页 0x07、按键盘页 0x0C）为规范编码；各平台映射表在实现侧。
- 文本输入（IME/Unicode）单独 `TextCommit{text}`，与原始键事件互斥（注入端抑制同批按键）。

## 7. 权限与策略
- 权限位：`VIEW`、`CONTROL`（输入）、`CLIPBOARD`、`FILE`、`AUDIO_OUT`、`MIC_IN`、`TERMINAL`、`SYSTEM_ACTION`、`PRIVACY_SCREEN`。
- 默认：`VIEW|CONTROL|CLIPBOARD`；协助模式默认 `VIEW|CONTROL` 且临时。
- 每次会话可选「仅本次/始终允许/拒绝」；密码错误限速（5 次/5 分钟锁定）。
- 被控端可强制：隐私屏（黑屏物理显示器）、水印、剪贴板禁用、文件根目录白名单。

## 8. 剪贴板（拉模式）
1. 监听方检测变化 → 计算内容 `hash = BLAKE3(格式名 ‖ 内容)`，发送 `ClipboardFormatList{seq, items[{id, hash, size_hint, name}]}`。
2. 对端按优先级（文本 > HTML > PNG > 文件列表）选一项 → `ClipboardPull{hash}`。
3. 提供方 → `ClipboardDataChunk{hash, offset, data, last}` 分块（默认 128 KiB），逐块连续发送；接收方校验 offset 单调与总长上限（1 MiB）。
4. 接收方写回本地剪贴板后回 `Applied{hash}`；来源端据此忽略自身 hash 触发的事件（防环），配合本地 `LoopGuard` 与序列号双重抑制。
5. 大对象（>1 MiB）或文件列表 → 走 `file-meta/file-data` 通道，剪贴板只传描述符（v1 后续）。
6. **v1 传输承载**：这四类消息作为 `Envelope` 的 tag 13–16 复用 `control` 双向流（可靠、有序、低速率）；通道表里的 `clipboard` 双向流 4 保留给后续大对象/独立流控，实现前不启用。
7. 规则引擎（可选）：目标 App 匹配 → 纯文本化/禁格式/禁图片/大小上限。

## 9. 文件传输
- `Offer{task_id, files[{rel_path,size,mtime,type}], root_id}` → 接收端 `Accept{resume_map}` / `Reject{code}`。
- `Block{task_id, block_id, offset}` + `BlockAck{block_id, len}`（≤3 未确认）；块长 1–4MiB；整文件 BLAKE3 校验。
- 策略：`OVERWRITE|SKIP|RENAME|RESUME`；沙箱根校验（拒绝 `..`/符号链接逃逸/绝对路径）；原子落盘。
- 配额：单文件/总量/速率；目录遍历单独 `ListDir{path}`/`Stat{path}`（只读元数据通道）。

## 10. 信令 REST/WSS（v1 最小集）
- `POST /v1/devices/{id}/register` → `{device_id, relay_ticket, stun_servers[]}`（Ed25519 签名请求体）。
- `GET /v1/devices/{id}/online` → `{online, last_seen, platform}`。
- `POST /v1/pair/assist` / `POST /v1/pair/confirm`（协助码发放/确认）。
- `WSS /v1/signal`：`Offer{session_id, candidates[]}`/`Answer`/`IceCandidate`/`RelayAlloc`/`Bye`。
- 中继：`POST /v1/relay/alloc` → `{relay_addr, session_tag, expires}`；Relay 按 `session_tag` 转发 UDP 密文。
- 消息全部 protobuf（调试可用 JSON，同一 schema）。

## 11. 可扩展性
- 未知消息类型：按 `type` 高位标记 `0x8000` 的 optional 段忽略；必需扩展须协商 `proto_ext[]`。
- 能力矩阵内的新功能以 `feature_flag` 位下发（参考 UU 的 ability map 思路，自研位定义），实现"新老客户端混连"分支。
- 版本：`proto_major` 不兼容变更才升级；`proto_minor` 追加字段/消息。

## 12. 待定（Open Questions）
1. TCP443 回退的复用帧是否走 relay 与 Agent 同一端口（TLS ALPN 区分？）。
2. 音频主时钟 vs 视频主时钟在 v1 选用（建议音频主时钟，简化 A/V 同步）。
3. 虚拟显示器与"super screen"（超大画布 + 视口漫游）是否进入 v1 协议（建议：v1 只做单屏/多屏切换，v2 加入）。
4. 中继是否支持 TURN 兼容（企业已有 TURN 环境接入）。