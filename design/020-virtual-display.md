# 020 — 虚拟显示器模块（Virtual Display）

状态：设计 + 第一阶段骨架（2025-11，Windows IddCx 用户态通信层已实现并测试；驱动为可评审
骨架，未编译、未签名、未安装）。
对应 crate：`crates/dfy-vdisplay`。驱动工程：`crates/dfy-vdisplay/driver/`。
相关文档：[`012-module-apis.md`](012-module-apis.md)（crate 契约风格）、
[`research/030-uu-display-input-audio.md`](../research/030-uu-display-input-audio.md)（UU 逆向）、
[`research/050-oss-base-and-arch.md`](../research/050-oss-base-and-arch.md) §5.5/§7（四 OS 选型）。

## 1. 目的与范围

远控的核心场景之一是「被控端没有显示器 / 不想占用物理显示器」。虚拟显示器模块负责：

1. **无头创建虚拟桌面**：主机没有任何物理显示器（或物理屏已关闭）时，虚拟显示器仍能让 Windows
   存在一个可渲染、可采集、可交互的显示目标。
2. **多个虚拟屏独立控制**：每个虚拟屏可独立设置分辨率/刷新率/DPI/桌面位置，互不影响。
3. **远程改分辨率不影响物理屏**：远程客户端修改虚拟屏分辨率时，物理显示器的模式与拓扑不得变化。
4. **跨 OS 统一抽象**：上游（`dfy-session`、`dfy-cli`、agent）只看到一个
   `VirtualDisplayBackend` trait，不感知平台。
5. **驱动异常自动恢复**：驱动/宿进程崩溃、设备重启、GPU 重置后，agent 能重建虚拟屏，并把正在
   进行的会话迁移到新的 display id。

本模块是**独立核心模块**：它不依赖采集、编码、协议，只提供「虚拟显示器的存在与形状」。采集
（`dfy-capture`）、每屏一路编码（`dfy-session`）、输入坐标映射（`dfy-input`）通过 display id
与它解耦地协作（见 §9）。

非目标（本阶段）：HDR、旋转、硬件光标、独立桌面（Windows 只有跨屏统一的桌面空间，"独立控制"
指每屏的模式/拓扑/编码流独立，而不是每屏一个 desktop 对象）、mac/Linux 的完整实现。

## 2. 决策摘要

| 决策 | 内容 | 理由 |
|---|---|---|
| D1 | 虚拟屏是独立 crate `dfy-vdisplay`，不并入 capture/session | 生命周期、权限（驱动/服务）、崩溃恢复语义都不同；独立 crate 才能被 agent 作为「设备管理器」单点持有 |
| D2 | Windows 走自研 IddCx UMDF2 驱动 | 无用户态替代方案；SudoVDA / RustDesk / UU 均已被验证可产品化；许可证干净（自研，PolyForm Noncommercial License 1.0.0） |
| D3 | Linux 走 evdi（GPL-2.0 内核模块独立分发）+ libevdi 动态链接，Wayland 场景补 DRM lease | evdi 是 DisplayLink 官方内核模块，事实标准；GPL-2.0 只约束内核模块，独立分发并在用户态 dlopen libevdi（LGPL-2.1）需与 PolyForm Noncommercial License 1.0.0 的分发条款逐项确认 |
| D4 | macOS 走 CGVirtualDisplay（私有 API） | 无公开 API；App Store 不可用，Developer ID 直发；接受其脆弱性与版本适配成本 |
| D5 | 用户态-驱动协议**自定**，不复用 UU/RustDesk 的 IOCTL 数值 | 同一台机器可能同时装了 UU/RustDesk 的 IddCx 驱动，数值必须区分；clean-room，不拷贝 SudoVDA 代码 |
| D6 | display id == ConnectorIndex（0..7） | 驱动重载/设备重启后连接器编号稳定；与 OS 回调中的 ConnectorIndex 双向一致，映射零成本 |
| D7 | 非 Windows 平台 `create_backend()` 返回 `UnsupportedBackend` 而不是 `Err` | 上游只有一套代码路径；`is_available()==false` 显式降级，且 stub 可在所有平台单测 |
| D8 | 相位 1 只做 IddCx 1.2（`UmdfExtensions=IddCx0102`） | Win10 1809+ 全覆盖；1.10 才支持 HDR，且 1.10 的扩展在 22621.2359 以下不加载，不能作为基线 |
| D9 | 驱动不持久化「期望配置」，agent 是唯一状态源 | 崩溃恢复只需重放 create/update（§8）；驱动侧无状态，避免驱动与 agent 状态不一致 |

## 3. 跨平台方案对比

| 维度 | Windows IddCx（UMDF2） | Linux evdi | Linux DRM lease | macOS CGVirtualDisplay |
|---|---|---|---|---|
| 机制 | 间接显示驱动（IddCx 1.2） | DisplayLink 内核模块 + libevdi 用户态 | DRM lease 把某个 connector 租给用户态进程 | CoreGraphics 私有 API |
| 真扩展屏 | 是（OS 认可为显示器，可作主屏/扩展屏） | 是（创建虚拟 connector） | 否：租用的是**已有** connector，不能凭空增加屏幕 | 是 |
| 无物理屏可用 | 是（唯一显示目标也可以） | 是 | 否（无 connector 可租） | 是 |
| 多屏独立 | 是（ConnectorIndex 0..N，每屏独立 swapchain） | 是（每 device 一个屏幕，可多 device） | 受硬件 connector 数限制 | 是（每个 display 一个对象） |
| 动态改模式 | 是（`IddCxMonitorUpdateModes`，1.2 即可） | 是（libevdi 模式列表） | 受 connector 能力限制 | 是（需重建 display 或改 mode） |
| 许可证/分发 | 自研驱动 PolyForm Noncommercial 1.0.0；需 EV/WHQL 签名 | 内核模块 GPL-2.0（独立分发，DKMS/MOK），用户态库 LGPL-2.1 动态链接 | 内核自带（GPL-2.0），无额外模块 | 私有 API，Developer ID 直发，App Store 拒绝 |
| 主要风险 | WHQL 2–4 周日历时间；HDR 需 1.10 | Secure Boot 下 MOK 注册；Wayland 合成器忽略 evdi；内核崩溃风险 | 不满足无头/多虚拟屏需求 | 私有 API 随 OS 更新失效；无官方支持 |
| 结论 | **产品化主路径** | Win 之外 Linux 主路径 | 仅作 Linux「已有屏幕复用」补充 | mac 唯一路径，接受脆弱性 |

补充事实（研究 050 §5.5/§7 + 本阶段核对）：

- UU / RustDesk 的 IddCx 驱动均在生产环境验证了「IndirectKmd + IddCx 1.2 + 动态模式表」路线；
  IddCx 是 Windows 上唯一被 Microsoft 支持的真扩展屏机制。
- evdi 的 GPL-2.0 只约束**内核模块**；用户态通过 `dlopen("libevdi.so")` 调用，不产生链接传染，
  模块本身以独立包（DKMS）分发即可。
- CGVirtualDisplay 是私有 API：能创建虚拟屏，但需要 `CGDisplay` 相关私有符号；mac 的屏幕录制
  权限（ScreenCaptureKit）与虚拟屏采集叠加时另有隐私授权复杂度。

## 4. 统一抽象（`crates/dfy-vdisplay`）

### 4.1 契约

```rust
// crates/dfy-vdisplay/src/lib.rs
pub struct VirtualDisplaySpec {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub dpi: u32,
    pub name_hint: String,     // 仅 ASCII graphic/空格，≤63 字节；驱动侧截断到 64B（含 NUL）
}

pub struct VirtualDisplayInfo {
    pub id: u32,               // Windows: ConnectorIndex；Linux/mac: 后端内稳定编号
    pub spec: VirtualDisplaySpec,
    pub attached: bool,        // 驱动已向 OS 通告该 monitor（可采集）
}

pub trait VirtualDisplayBackend: Send + Sync {
    fn list(&self) -> anyhow::Result<Vec<VirtualDisplayInfo>>;
    fn create(&self, spec: &VirtualDisplaySpec) -> anyhow::Result<u32>;
    fn update(&self, id: u32, spec: &VirtualDisplaySpec) -> anyhow::Result<()>;
    fn destroy(&self, id: u32) -> anyhow::Result<()>;
    fn is_available(&self) -> bool;
    /// 后端能力位图，默认 0；Windows 驱动上报 capability::* 位。
    fn capabilities(&self) -> u32 { 0 }
}

pub fn create_backend() -> anyhow::Result<Box<dyn VirtualDisplayBackend>>;
```

能力位（`capability`）：`MULTI_MONITOR=1<<0`、`DYNAMIC_MODES=1<<1`、`HOTPLUG_EVENTS=1<<2`、
`HDR=1<<3`（Windows 1.2 不上报 HDR）。

### 4.2 语义约定

- **不做隐式创建**：`list()` 只报告 `create()` 创建过的屏幕；不会把物理屏混进来。
- **id 稳定性**：同一后端实例生命周期内稳定；驱动重启后可能变化（Windows 上通常不变，见 §8.4），
  上层必须通过 `list()` 重新对齐，不得缓存跨崩溃。
- **校验在库内**：`create/update` 先跑 `validate_spec()`（宽高偶数且在 16..=16384、刷新 1..=360、
  DPI 24..=480、name_hint 长度与字符集），违规返回带全部原因的 `SpecViolations`，**不发 IOCTL**。
  驱动侧 `DfyValidateMode()` 做同样的边界检查（双保险，驱动是最终裁决者）。
- **错误模型**：全部 `anyhow::Result`；不加 `thiserror`（仓库现状无该依赖，见
  `design/012-module-apis.md`）。错误消息要能指导现场排障（区分「设备不存在」/「驱动拒绝了参数」/
  「协议版本不匹配」）。
- **panic 禁令**：正常错误路径不 panic；unsafe 仅限 Win32 FFI 边界，均带 SAFETY 注释。
- **线程模型**：traits 要求 `Send + Sync`，后端内部用 `std::sync::Mutex` 串行化设备句柄；
  `wait_for_change()` 在锁外等待具名事件（见 §5.3），不阻塞其它调用。
- **降级**：非 Windows `create_backend()` 返回 `UnsupportedBackend`（`is_available()==false`，
  所有操作返回可读的 `Err`），而不是直接 `Err`——上层可以在 Windows/mac/Linux 上用同一段代码
  做「有无虚拟屏能力」的探测。

## 5. 用户态-驱动协议（Windows）

这是本模块的核心契约：`src/win_driver.rs`（用户态）与 `driver/driver.h`（驱动）必须逐字节一致，
两侧都有测试/`C_ASSERT` 守护。

### 5.1 设备接口

- 设备接口类 GUID：`{54EC77FA-1377-44E6-8C32-88FD5F44C84C}`（间接显示驱动使用该类的既有事实；
  我们的设备实例路径是 `\\?\root#dfyvdisplay#...`，可与 UU/RustDesk 的驱动区分）。
- 枚举：`SetupDiGetClassDevsW(&GUID, NULL, NULL, DIGCF_PRESENT|DIGCF_DEVICEINTERFACE)` →
  `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetailW` → `CreateFileW`。
- 打开权限：INF 的安全描述符 `D:P(A;;GA;;;BA)(A;;GA;;;SY)(A;;GA;;;UD)` 允许普通用户打开，
  agent 不需要管理员即可 `create/update/destroy`；安装驱动本身需要管理员（§9）。
- 协议版本：`GET_PROTOCOL_INFO` 返回 `{protocol_version, capabilities}`；用户态仅在
  `protocol_version == 1` 时视为可用，版本不匹配即 `is_available()==false` 并给出明确错误。

### 5.2 IOCTL（`CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800+n, METHOD_BUFFERED, FILE_ANY_ACCESS)`）

| 名称 | 值 | 输入 | 输出 | 语义 |
|---|---|---|---|---|
| `IOCTL_VDISPLAY_CREATE` | `0x222000` | `VDISPLAY_CREATE`（80 B） | `UINT32` id | 占用一个 connector、创建并通告 monitor |
| `IOCTL_VDISPLAY_UPDATE` | `0x222004` | `VDISPLAY_UPDATE`（20 B） | — | 改模式表并 `IddCxMonitorUpdateModes` |
| `IOCTL_VDISPLAY_DESTROY` | `0x222008` | `VDISPLAY_DESTROY`（4 B） | — | 停 swapchain、`IddCxMonitorDeparture`、释放 connector |
| `IOCTL_VDISPLAY_LIST` | `0x22200C` | — | `VDISPLAY_STATE_LIST`（200 B） | 全量状态（权威来源） |
| `IOCTL_VDISPLAY_TAKE_EVENT` | `0x222010` | — | `VDISPLAY_EVENT`（8 B）或 0 B | 取一条待处理事件；0 B 表示无事件（成功） |
| `IOCTL_VDISPLAY_GET_PROTOCOL_INFO` | `0x222014` | — | `VDISPLAY_PROTOCOL_INFO`（8 B） | 版本与能力协商 |

注意数值是 `FILE_DEVICE_UNKNOWN(0x22)` + function `0x800+n` + `METHOD_BUFFERED(0)` +
`FILE_ANY_ACCESS(0)` 的结果，**故意不同于** UU/RustDesk 的 `0x30C004/8/C`
（`FILE_DEVICE_CHANGER` + `0x1001..` + `FILE_READ|FILE_WRITE`），避免两台驱动在协议层互相误认。

### 5.3 共享结构体（`#[repr(C)]` / `#pragma pack(4)`，两侧 size/align 断言）

```c
// 80 bytes: 4+4+4+4+64；name 是 NUL 结尾 UTF-8，超出 63 字节按字符边界截断
typedef struct { UINT32 Width, Height, RefreshHz, Dpi; UINT8 NameHint[64]; } VDISPLAY_CREATE;
// 20 bytes
typedef struct { UINT32 Id, Width, Height, RefreshHz, Dpi; } VDISPLAY_UPDATE;
// 4 bytes
typedef struct { UINT32 Id; } VDISPLAY_DESTROY;
// 8 bytes
typedef struct { UINT32 Kind, Id; } VDISPLAY_EVENT;
// 200 bytes: Header{Count,Reserved} + States[8]{Id,W,H,RefreshHz,Dpi,Attached}
typedef struct { ... } VDISPLAY_STATE_LIST;
// 8 bytes
typedef struct { UINT32 ProtocolVersion, Capabilities; } VDISPLAY_PROTOCOL_INFO;
```

- 结构体不含指针、不含 `size_t`、不用 `bool`（用 `UINT32`），保证 32/64 位与 Rust 侧一致。
- `LIST` 固定 200 字节（不随实际屏数变化）：用户态用定长缓冲一次读全量，解析时校验
  `返回字节数 == 200 && Header.Count <= 8`，并过滤 `INVALID_ID`。
- 驱动不回传 `name_hint`（`VDISPLAY_STATE` 只有 24 B）：用户态在 `create` 时本地缓存 id→name，
  崩溃重连后 name 丢失是可接受的（name 只是 UI 提示）。

### 5.4 事件协议

双层设计，与 UU 逆向结论一致（具名事件做通知、IOCTL 做数据）：

1. **具名事件** `DafeiyuVDisplayDeviceEvent`（auto-reset）：驱动在 monitor 到达/离开、
   模式表变化时 `SetEvent`。用户态 `CreateEventW` 打开后 `WaitForSingleObject(timeout)`，
   `WAIT_OBJECT_0` 表示"有变化"，随后 drain。
2. **事件环** `VdisplayEvent{kind,id}`：驱动侧 64 条环形缓冲，用户态用 `TAKE_EVENT` 逐条取。
   `kind ∈ {MONITOR_ARRIVAL=1, MONITOR_DEPARTURE=2, MODES_CHANGED=3}`。

事件是**优化**不是权威：任何一条事件丢失（环形覆盖、驱动重载）后，`list()` 仍能恢复全量状态。
用户态 `wait_for_change(timeout_ms) -> Result<Option<VdisplayEvent>>`：超时返回 `Ok(None)`，
断连返回 `Err`，由上层 recovery（§8）处理。

### 5.5 错误映射

| 场景 | 驱动返回 | 用户态表现 |
|---|---|---|
| connector 用尽（>8） | `STATUS_INSUFFICIENT_RESOURCES` | `Err("... all 8 connectors are in use ...")` |
| id 不存在 / 未创建 | `STATUS_INVALID_PARAMETER` | `Err("... display id N is not known to the driver")` |
| spec 越界 | `STATUS_INVALID_PARAMETER` | 库内校验先行，通常不会到驱动；驱动侧仍兜底 |
| adapter 未就绪 | `STATUS_DEVICE_NOT_READY` | `Err("... driver is still initializing")` |
| 设备被卸载/停用 | `ERROR_INVALID_HANDLE` / `ERROR_FILE_NOT_FOUND` / `ERROR_NO_SUCH_DEVICE` / `ERROR_DEVICE_NOT_CONNECTED` | 清空句柄并返回 `Err`；下次调用重新枚举（自动重连） |
| 协议版本不符 | `ProtocolVersion != 1` | `is_available()==false` |

## 6. 驱动侧设计（IddCx 1.2 / UMDF2）

### 6.1 组成与回调

`driver/driver.c` 骨架已给出完整控制面（设备创建、设备接口、6 个 IOCTL、monitor/swapchain 生命周期、
事件环），像素路径与模式描述回调是 TODO（细节见 `driver/README.md` §3）。

关键点：

- `IddCxDeviceInitConfig` 在 `WdfDeviceCreate` 之前调用：**IddCx 接管设备队列，
  `IRP_MJ_DEVICE_CONTROL` 被重定向到 `EvtIddCxDeviceIoControl`**，普通 `EvtIoDeviceControl`
  永远收不到我们的 IOCTL。这点是 IddCx 新手最容易踩的坑。
- Adapter 在 `EvtDeviceD0Entry` 里 `IddCxAdapterInitAsync` 异步初始化；完成后
  `EvtIddCxAdapterInitFinished` 拿到 `IDDCX_ADAPTER`，此后 `CREATE` 才能成功。
- Adapter 的 `EndPointDiagnostics`：`TransmissionType=WIRED_OTHER`、
  `GammaSupport=NONE`、`FriendlyName="Dafeiyu Virtual Display"`、
  `Manufacturer="Dafeiyu"`、`Model="Dafeiyu VDisplay"`，`MaxMonitorsSupported=8`。
- 每个 connector 槽位（`DFY_DISPLAY_SLOT`）是驱动侧唯一状态：模式表（当前模式 + 4 个兜底模式）、
  EDID（128 B，身份 + preferred timing）、`IDDCX_MONITOR`、swapchain worker。
- EDID 只负责身份与 preferred timing；**模式表不由 EDID 决定**，而是 `QUERY_TARGET_MODES`
  回调 + `IddCxMonitorUpdateModes` 动态注入（与 UU 逆向结论一致：EDID 静态、模式运行时可改）。

### 6.2 版本分层（重要限制）

| 能力 | IddCx 1.2（本包） | IddCx 1.10（后续） |
|---|---|---|
| 动态模式表 | `IddCxMonitorUpdateModes` + `IDARG_IN_UPDATEMODES` | 同左 + `IddCxMonitorUpdateModes2`（`IDARG_IN_UPDATEMODES2`，`IDDCX_TARGET_MODE2`） |
| HDR | 不支持（`GammaSupport=NONE`） | 支持（`IddCxMonitorSetDefaultHdrMetaData` 等） |
| 最低 OS | Windows 10 1809+ | Windows 11 22H2 September Update（22621.2359+） |
| INF | `UmdfExtensions=IddCx0102` | 需 `IddCx0110`，且旧 OS 不加载 |

因此**不能**在 1.2 包中调用 `IddCxMonitorUpdateModes2`（导入缺失会上不了屏，甚至加载失败）。
HDR 阶段的路线：保留 1.2 包为基线，另建 1.10 变体 + 按 OS build 选包的安装器（或单二进制 +
`GetProcAddress(IddCx.dll, "IddCxMonitorUpdateModes2")` + `IddCxGetVersion` 运行时选路）。

### 6.3 每屏独立 swapchain 与像素路径

OS 为每个已 commit 的虚拟显示器各分配一条 swapchain，通过
`EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN` 交给驱动：

1. 驱动把 `hSwapChain` / `hNextSurfaceAvailable` / `RenderAdapterLuid` 存进该 connector 的
   worker 结构，起一条专属线程（**回调内不阻塞**）。
2. 线程用 `RenderAdapterLuid` 建 D3D11 设备，`IddCxSwapChainSetDevice` 绑定。
3. 循环 `IddCxSwapChainReleaseAndAcquireBuffer`：无新帧 → `WaitForMultipleObjects
   {newFrame, terminate}`（16 ms 上限）；有新帧 → 处理 `MetaData.pSurface`
   （`IDXGIResource`）→ `IddCxSwapChainFinishedProcessingFrame`。
4. `UNASSIGN`：置 terminate 事件，join，**线程最后动作是 `WdfObjectDelete(hSwapChain)`**，
   让 OS 下次重新分配新链。少这一步 = 下次 assign 失败/黑屏，是这一类驱动最常见的事故。
5. 生命周期铁律：swapchain 句柄只属于 worker 线程；`FinishedProcessingFrame` 之后不得再碰帧。

相位 1 的像素路径可以先「取到帧就丢弃」（验证链路管用），采集由 `dfy-capture` 用 DXGI 在
虚拟屏的 output 上做（§9），驱动不必把帧送进用户态——这是有意的分层：驱动只提供屏幕，
不承担编码/传输职责。

## 7. 多虚拟屏与「远程改分辨率不影响物理屏」

### 7.1 多虚拟屏

- 一个 adapter、8 个 connector：`CREATE` 占用槽位 i → `IddCxMonitorCreate(ConnectorIndex=i)` →
  `IddCxMonitorArrival`。每屏有独立 container GUID、独立模式表、独立 swapchain、独立 worker。
- 我们**不参与拓扑**：位置/主屏/缩放/旋转全部交给 Windows CCD API
  （`DisplayConfigGetDeviceInfo` / `SetDisplayConfig`），由 agent 的会话层调用。驱动只在
  `COMMIT_MODES` 里收到"OS 已选定模式"的通知，不做动作。
- 「独立控制」的准确含义：每屏的模式、DPI、位置、编码流独立；Windows 仍只有一个跨屏桌面
  （没有 per-monitor desktop 对象）。这一点必须在产品文案里说清，避免承诺做不到的事。

### 7.2 为什么改分辨率不影响物理屏

三层隔离：

1. **驱动实例隔离**：我们的 adapter 只拥有自己的 connector；物理输出属于系统显示适配器，
   不在这个 adapter 的 target 列表里。
2. **模式表隔离**：`UPDATE` → `IddCxMonitorUpdateModes` 只更新该虚拟 monitor 的 target modes，
   OS 重新 commit 时只可能改这条 path；物理 output 的 path 不在本次 commit 的目标集合内。
3. **拓扑隔离**：远程端改分辨率时，agent 只对虚拟 display 调用模式设置；物理屏的
   `DISPLAYCONFIG_PATH_INFO` 不被写入。即使 agent 做了一次全量 `SetDisplayConfig`，
   也以「读-改-写虚拟 path」的方式构造，物理 path 原样保留。

边界情况：若虚拟屏被设为主屏，Windows 可能因主屏模式变化重排副屏坐标（位置会变，模式不变）。
这在 §7.1 的「拓扑由 agent 控制」范围内，agent 应在改模式后恢复布局。

## 8. 驱动崩溃与异常恢复

### 8.1 故障模型

| 故障 | 现象 | 检测 |
|---|---|---|
| UMDF 宿进程崩溃（驱动 bug） | `WUDFHost.exe` 重启，设备进入 `Device problem`；句柄全部失效 | `DeviceIoControl` 返回断连错误；`is_available()` 失败 |
| 驱动被 PnP 重启（更新/禁用后启用） | 设备重新 D0，adapter 重新初始化，monitor 需要重建 | `wait_for_change` 超时/断连；`list()` 为空 |
| GPU 重置/TDR | swapchain worker 的 D3D 调用失败，屏幕卡住 | worker 检测 `DXGI_ERROR_DEVICE_REMOVED` → 结束线程（并删 swapchain）；agent 侧无事件也应由 watchdog 触发重建 |
| 驱动卸载 | 接口消失，屏全部消失 | 枚举不到设备 |

### 8.2 agent 侧状态机（建议实现于 `dfy-session` 或 agent 的 display supervisor）

```
Healthy --(ioctl 断连 / is_available=false)--> Degraded
Degraded --(re-enumerate; 设备重新出现且 protocol 版本匹配)--> Recovering
Recovering --(list() 对账)--> 
    - 屏仍在、id 未变  -> 补 update()，回 Healthy
    - 屏仍在、id 变了  -> 按 name_hint + 原顺序做 id 迁移（§8.4），补 update()，回 Healthy
    - 屏丢失          -> 对每个会话重新 create()，迁移 display id，回 Healthy
```

- **watchdog**：`wait_for_change(1000)` 循环 + 每 N 次（如 30 s）一次 `is_available()`/`list()`
  主动探活；连续 3 次失败进入 `Degraded`。
- **退避**：重试指数退避 250 ms → 2 s → 5 s 上限，避免驱动加载中反复打设备。
- **连接重建**：`WinDriverBackend` 内部在断连错误时清空句柄；下次任意调用重新枚举 + 重开
  （已经实现，测试覆盖「无驱动时降级」路径）。
- **状态可重放**：驱动无持久状态（D9），恢复时 agent 用内存中的 desired 配置重放
  `create/update` 即可；不需要驱动侧持久化。

### 8.3 不要在恢复路径上 panic

断连、句柄失效、驱动未安装都是**正常的运行态**（尤其客户端机器可能根本没装驱动）：
`list()` 应返回 `Err` 或空列表 + 明确原因，调用方决定 UI 呈现；不得 `unwrap()`。

### 8.4 会话迁移到新 display id

display id 在 Windows 上等于 ConnectorIndex，PnP 重启后通常不变；但重装驱动、换 GPU、
改了 connector 上限时可能改变。迁移策略：

1. **稳定键**：会话以 `(display_id, name_hint, 创建顺序)` 三元组标识屏幕；name_hint 由 agent
   写死（如 `"Dafeiyu Display A"`），在多屏场景下足以区分。
2. **对账**：恢复后 `list()` 得到新集合；按 name_hint 配对旧→新 id，更新
   `session.display_id`，重新绑定 `dfy-capture` 的采集源与 `dfy-session` 的编码管线。
3. **无法配对**（名字被驱动截断/丢失）：按 `创建顺序 + 分辨率 + connector 顺序` 兜底；
   仍失败则销毁旧会话并按新屏重建（用户可见的短暂黑屏，可接受）。
4. **采集侧重绑**：`dfy-capture` 通过 monitor_id 选屏；迁移时对旧 capture 调 `recreate()`，
   避免缓存已失效的 DXGI output。

## 9. 与其它模块的集成点

```rust
// agent 侧伪代码
let vd = dfy_vdisplay::create_backend()?;
if !vd.is_available() { /* 提示：未安装虚拟显示驱动 / 非 Windows */ }

// 1) 建屏（分辨率来自客户端或配置）
let spec = VirtualDisplaySpec::new(1920, 1080, 60, 96, "Dafeiyu Display A")?;
let display_id = vd.create(&spec)?;

// 2) 采集：dfy-capture 以 monitor_id 选择 DXGI output（虚拟屏就是普通 monitor）
let capturer = dfy_capture::create_capturer(display_id as u32)?;   // monitor id 映射见下
// 3) 编码：每屏一路
let mut session = dfy_session::Session::for_display(display_id, codec)?;

// 4) 远程改分辨率
vd.update(display_id, &VirtualDisplaySpec::new(2560, 1440, 60, 96, name)?)?;
// dfy-capture 收到 DXGI output 变化后 recreate()；dfy-session 重建编码器（关键帧强制）
```

- **`dfy-capture`**：虚拟屏在 OS 看来是普通 monitor，`create_capturer(monitor_id)` 直接可用。
  需要对齐的只有 id 空间：`dfy-capture::MonitorInfo.id` 来自 DXGI/`EnumDisplayMonitors`，与
  `dfy-vdisplay` 的 display id 不是同一个编号。建议在 agent 层做一次映射：
  `display_id -> device name/位置 -> DXGI output`，映射表随 §8.4 的迁移更新。**不要**把
  `dfy-vdisplay` 的 id 直接当 `dfy-capture` 的 id 用（第一阶段最容易错的地方）。
  另外分辨率变化必须触发 `recreate()`（DXGI output 尺寸变了，旧纹理大小不对）。
- **`dfy-session`**：每屏一路编码，分辨率来自虚拟屏的 spec；`update()` 成功后强制关键帧，
  否则客户端会看到花屏直到下一个 I 帧。
- **`dfy-cli`**：`dfy vdisplay list|create|update|destroy` 便于真机调试与自动化测试。
- **`dfy-input`**：绝对坐标按屏映射；多虚拟屏时输入坐标以「屏内归一化 + display id」传输，
  避免 Windows 负坐标/主屏偏移带来的歧义。
- **`dfy-proto`**：能力协商里加「被控端支持虚拟屏数量/是否支持动态改分辨率」标志，客户端据此
  显示可选项；协议版本升级时同步 `DFY_VDISPLAY_PROTOCOL_VERSION`（驱动协议独立于 DFYP 版本）。

## 10. 里程碑

| 里程碑 | 内容 | 预估 |
|---|---|---|
| M3.5（驱动骨架） | ✅ crate + 用户态通信层 + 驱动骨架（本次交付） | 已完成 |
| M4 前：**驱动安装 + 单虚拟屏 1080p** | 生成 vcxproj、编译通过、EDID 校验、模式回调、swapchain 取帧丢弃、测试签名 + testsigning 机器上安装；单屏 1920x1080@60 在远程端可见并可采集 | 3–5 周（含驱动环境搭建 1 周） |
| M4 | 多虚拟屏（2–8）、动态分辨率（`update` 端到端）、`dfy-capture`/`dfy-session` 集成、拓扑设置 | 2 周（依赖 M4 前） |
| M5 | 崩溃恢复（watchdog + 会话迁移）、安装器（`pnputil`）、日志/诊断、EV 签名 | 2 周 + 签名日历时间 |
| 发布 | HLK + WHQL 提交（2–4 周日历） | 与 M5 并行启动 |
| Win 阶段 2 | IddCx 1.10 变体 + HDR | 后续 |
| Linux/mac | evdi/libevdi 后端、CGVirtualDisplay 后端 | M6+ |

「M4 前」这一档刻意只承诺单屏 1080p：它把**驱动能被 Windows 接受**这个最大不确定性提前验证，
同时不阻塞采集/编码/协议的并行开发（它们可以用 `vdisplay` 的 stub + 假 backend 单测）。

## 11. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| WHQL/EV 签名日历时间 2–4 周 | 发布延期 | M4 前先用 testsigning 内部版本；EV 证书立即启动申请；HLK 提前跑到无 fail |
| IddCx 1.2 无 HDR | 高端场景体验 | 1.10 变体按 OS build 分发（§6.2），不牺牲 Win10 兼容 |
| 驱动 bug → 蓝屏（`IndirectKmd` 在内核） | 致命口碑 | 严格遵守「回调不阻塞」「swapchain 线程独占、退出即删」「teardown join」；WdfVerifier + 内核调试；灰度发布 |
| 跨 GPU 采集（虚拟屏渲染在 A 卡、采集在 B 卡） | 采集回拷开销 | OS 选择 render adapter LUID；采集侧用 DXGI 跨适配器纹理拷贝或强制同 GPU；性能数据进 M4 验证项 |
| 边缘：虚拟屏是唯一显示时驱动加载失败 | 黑屏无法操作 | 安装器先检查是否有物理屏；保留安全模式/远程管理兜底；驱动失败时 Windows 会回退 |
| mac 私有 API 失效 | mac 功能中断 | 版本适配层 + 后端降级到 stub（`is_available=false`），不阻塞远控主功能 |
| evdi 在 Wayland 合成器被忽略 | Linux 虚拟屏不可用 | 文档标注 X11 优先；Wayland 场景用 DRM lease/合成器特例，长期跟进 |
| 多驱动共存（UU/RustDesk 同时安装） | 接口混淆 | 我们的类 GUID 相同但设备实例不同；`looks_like_device_path()` 校验路径含我们的根枚举 id；协议数值不同，不会互相误解析 |

## 12. 参考

- Microsoft IddCx 文档：<https://learn.microsoft.com/windows-hardware/drivers/display/iddcx-objects>
  （`IDD_CX_CLIENT_CONFIG`、`IDARG_IN_UPDATEMODES(2)`、`IddCxSwapChain*`）
- `IddSampleDriver` 样例（MIT，仅结构参考）：<https://github.com/microsoft/Windows-driver-samples/tree/main/video/IndirectDisplay>
- SudoVDA（MIT，仅架构参考，未复制代码）：<https://github.com/virtual-display-project/virtual-display>
- evdi（GPL-2.0 内核模块 / LGPL-2.1 用户态）：<https://github.com/DisplayLink/evdi>
- CGVirtualDisplay（私有 API）参考实现：<https://github.com/KhaosT/CGVirtualDisplay>
- 本仓库：`research/030-uu-display-input-audio.md`（UU IddCx 逆向）、
  `research/050-oss-base-and-arch.md` §5.5/§7、`crates/dfy-vdisplay/driver/README.md`（驱动 runbook）