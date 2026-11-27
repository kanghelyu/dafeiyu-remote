# 021 · 剪贴板同步：拉模式协议与状态机

状态：已实现（`dfy-proto` 消息 + `dfy-clipboard` 状态机 + 单元测试），尚未接入 CLI 会话循环。
相关：`spec/DFYP-v1.md` §8、`design/011-mvp-acceptance.md`。

## 1. 为什么是"拉模式"

被控端（agent）**不主动推送剪贴板内容**，只广播"当前有哪些格式、各自指纹（BLAKE3）"，控制端（viewer）按需拉取。理由：

1. **带宽**：剪贴板里经常有大图/大文件列表，没人要就不该传。
2. **自激抑制**：两端都会监听本地剪贴板；"写回远端内容"会再次触发本地变更事件。带指纹的清单让写回方可标记 `Applied`，避免 A→B→A 无限回环。
3. **可续传**：分块拉取天然支持大对象、断点、限速。

UUX 的做法（见 `research/040-uu-features-ui.md` §4）也是同一思路（`ClipboardFormatListRequest` → `ClipboardFormatDataAsk` → `ClipboardDataBlock` + confirm 流控），我们做了 clean-room 重新设计：**清单里直接带内容指纹**，省掉一轮"问大小/问哈希"的往返。

## 2. 线上消息（`dfy-proto` tag 13–16）

| tag | 消息 | 方向 | 字段 |
|---|---|---|---|
| 13 | `ClipboardFormatList` | 双向（广播） | `seq:u64`（本端单调序列号）、`items: Vec<ClipboardFormat>` |
| 14 | `ClipboardPull` | 接收方→提供方 | `hash: [u8;32]`（要哪个对象的 BLAKE3） |
| 15 | `ClipboardDataChunk` | 提供方→接收方 | `hash:[u8;32]`、`offset:u64`、`data:bytes`、`last:bool` |
| 16 | `ClipboardApplied` | 接收方→提供方 | `hash:[u8;32]`（我已写回本地剪贴板，别再为此发清单） |

`ClipboardFormat { id: u32, hash: [u8;32], size_hint: u64, name: String }`：

- `id` 用来排序/去重与本地格式对应（Windows 上是 `CF_*` 常量或注册格式 id）。
- `hash` 是**内容**的 BLAKE3（不是 "格式名+长度"），所以同名不同内容必须重新拉取。
- `size_hint` 只用于决策（太大就不自动拉），不作为完整性依据。
- `name` 是格式名（`CF_UNICODETEXT` / `HTML Format` / `PNG` / `CF_HDROP` / `Rich Text Format` / 自定义名）。

走 QUIC **双向流** `CHAN_CLIPBOARD = 4`（`dfy-proto::consts`），不是 DATAGRAM：剪贴板对可靠性敏感、对时延不敏感。

## 3. 状态机（`dfy-clipboard::ClipboardSync`）

```
本地剪贴板变更 (watcher)
      │  poll_local()
      ▼
 读当前所有格式 → 逐个计算 BLAKE3 → 与上次发布签名比对
      │ 有变化
      ▼
  发 ClipboardFormatList（带 seq）
```

```
收到 ClipboardFormatList
      │ 忽略自己刚发过的 seq / 忽略已发布签名
      ▼
 按优先级选一个格式（文本 > HTML > PNG > 文件列表）
      │ 不在已发布集合、未超上限
      ▼
   发 ClipboardPull{hash}
```

```
收到 ClipboardPull（我是提供方）
      ▼
 从本地读出该格式的全部字节 → 按 128 KiB 切块 → 连发 ClipboardDataChunk(last=false…last=true)
```

```
收到 ClipboardDataChunk（我是接收方）
      ▼
 按 hash 累积到缓冲（校验 offset 连续、总长不超 MAX_SYNC_BYTES）
      │ last=true
      ▼
 写回本地剪贴板 → 记入 published（note_write）→ 发 ClipboardApplied
```

关键常量（`dfy-clipboard/src/sync.rs`）：

- `MAX_SYNC_BYTES = 1 MiB` —— 超过就不自动同步（可配置，后续按会话权限位放开大文件走文件通道）。
- `CHUNK_BYTES = 128 KiB` —— 与 QUIC 流窗口/内存占用折中。
- 去重签名 = `BLAKE3(格式名 ‖ 内容)`；`LoopGuard` 仍是最后一道闸（防止本地 API 写回引发的二次事件在 watcher 线程里回灌）。

## 4. 与安全模型的衔接

- 剪贴板同步受权限位 `CLIPBOARD` 控制（`spec/DFYP-v1.md` §7）；默认**仅文本**，图片/文件列表需要显式放开。
- 内容只在 E2E 通道内传输（TLS1.3 + 设备证书），中继只看得到密文。
- 不做跨会话剪贴板历史（不落盘）；`ClipboardApplied` 不带内容，避免"三次握手"式泄漏。
- 恶意方可以构造超大 `size_hint` 或无穷分块：接收侧按 `MAX_SYNC_BYTES` + `offset` 单调性拒绝，测试覆盖超限路径。

## 5. 平台后端（`ClipboardBackend`）

| 平台 | 实现 | 状态 |
|---|---|---|
| Windows | `win.rs`：`OpenClipboard` 重试 + RAII；`EnumClipboardFormats`；文本 UTF-16、`CF_DIB/CF_DIBV5`→PNG、`HTML Format`、`CF_HDROP`（`hdrop.rs`）、RTF；`AddClipboardFormatListener` + 消息专窗 watcher | 已实现、单测覆盖编解码 |
| Linux | X11：`XFixes` 选择属主 / `XConvertSelection`；Wayland：`wlr-data-control` 或 Portal（仅 `text/plain`） | X11 已实现，原生注入/剪贴板测试通过；Wayland 待实现 |
| macOS | `NSPasteboard`（`changeCount` 轮询）+ `NSPasteboardItem` 多类型 | 待实现 |
| Android | `ClipboardManager`（仅文本/URI，后台受限） | 待评估（Android 里程碑） |

## 6. 落地顺序

1. ✅ 协议消息 + 状态机 + 单测（本文件所述）。
2. ✅ 接入 `dfy-cli agent/view`：Agent 使用 `--allow-clipboard`，view 使用 `--clipboard`；均默认关闭，会话开始前的本地内容不自动上传。
3. ✅ 权限位协商：Welcome 的 `PERM_CLIPBOARD` 与实际后端能力一致，客户端未获授权时不发送清单。
4. ✅ Linux X11 后端；macOS 待实现。
5. ⏳ 大对象（>1 MiB）改走 `file-meta/file-data` 通道，剪贴板只传"引用"。

## 7. 接入实现要点（v1 现状）

- 会话层只加了两条通用缝：`CtrlHandler::on_tick()`（跟随 100ms ticker）与 `CtrlHandler::drain_outbox()`（一次 `on_ctrl` 可能产生多条回帧，例如分块）。
- `AgentHandler` 组合输入注入与剪贴板；两者可独立关闭，任一后端不可用只降级不中断会话。
- `ViewClipboard` 独立处理本地轮询与远端消息，返回待发帧由 `real.rs` 的 `select!` 发送。
- 消息复用 control 流（tag 13–16；通道 4 保留给将来的独立流）。
- 实机验证（同一台机器）：agent→view 文本 19B 与 view→agent 文本 20B 各闭环一次（清单 / 拉取 / 分块 / 确认 各 1），无回环。
- 未做：`--clipboard=text|all` 分级、macOS 后端、>1MiB 走文件通道。