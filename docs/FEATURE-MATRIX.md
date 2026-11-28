# 功能实现与验证证据矩阵

快照日期：2026-11-28。远端 macOS 基线为 `f55cbfb364cfa3f8082f133448c8984e6c66bba0`（`v0.2.0`），其上有本地原生 UI 重构 `ed8b8e0` 与全平台打磨；早期证据仍保留原起始 HEAD `7f1d67c1bd37e5323a4b3bcbac020097896c3708` 的上下文。需求以[完整原文](requirements/000-original-user-prompt.md)及[28个原文模块](requirements/COVERAGE.md)为准；本表不改变范围。

## 状态读法

- **实现代码**：阅读到实际逻辑；不等于已在目标OS运行。**接口/骨架**：只有契约、结构或平台stub，不能标作功能完成。
- **测试定义**：仓库有单测/集成测试；未附本轮命令结果时，不称本轮通过。**单测/loopback**不能代替真实平台或跨机测试。
- **历史本机**：旧文档记录了同机真实后端实验，但本轮未复跑；不沿用旧勾号。**本轮验证**仅认[STATUS验证记录](STATUS.md)中的命令、环境与结果。
- **跨机/跨平台**：必须记录两端环境、网络路径与业务验收；本轮已取得 Windows↔Debian QEMU 同机隔离 VM 的视频/指针/双向剪贴板/文件部分证据，但尚未获得两台独立物理设备或公网完整验收。

每个原文清单项仍须单独验收；一行汇总包含多项要求时，不能因其中一项可用而整行完成。

## 核心与平台

| 能力 | 需求模块 | 阅读到的实现状态 | 当前证据范围 | 尚缺的验收 |
| --- | --- | --- | --- | --- |
| Windows采集→编码→网络→显示 | 004/009/020 | WGC优先、DXGI初始化失败回退；MF H.264、QUIC、winit/softbuffer代码；保活Agent无活动Viewer时暂停内部采集，新会话清掉旧队列帧 | 历史本机E01；本轮安装版DXGI 10秒145重组/109解码；最终Release在GameViewer虚拟输出WGC→H.264→QUIC→API36 Android Viewer真实显示，Agent 25采集/25编码/9输出，Android 7组装/7提交；同一Debug Agent两次断开后采集统计分别稳定43/43与75/75，重连均有关键帧和已解码画面；Windows→Linux同机QEMU 26秒380/380解码、指针/双向剪贴板/51B文件部分通过；Linux→Windows同机QEMU 26秒204/204解码、指针/A键/双向剪贴板/50B文件通过，见STATUS | Android真机、两台独立设备、弱网、长稳、首帧延迟、硬件编码器、DXGI运行期无更新回退、Windows合成键钮采集、空闲能耗与迁移中不中断实测 |
| Linux主控与被控 | 004/020 | X11采集、JPEG及窗口共享层；Linux输入/剪贴板后端；Linux主控XInput2优先、X11根窗口核心事件降级，XKB物理名↔HID映射，10ms移动合并，停止热键释放 | 源码E03/E04/E05；Linux check、dfy-input 31单测、CLI 2单测＋1隔离Xvfb真实采集通过；Windows→Linux同机QEMU跨OS视频/指针/双向剪贴板/文件部分通过，见STATUS | 两台独立设备、Linux↔Linux全链路、真实桌面XInput2/焦点/IME/多屏、Linux原生Wayland；不能只交叉编译 |
| Linux原生Wayland | 004/006/007/008 | 输入/剪贴板明确拒绝；XWayland不等于全桌面授权采集 | 实现边界E03/E04/E05 | Portal/PipeWire等经授权原生路径、撤权/重启测试 |
| macOS主控与被控 | 004/020 | 已实现ScreenCaptureKit采集、VideoToolbox H.264编解码、HID↔CGKeyCode/`CGEvent`注入、`CGEventTap`主控采集、`NSPasteboard`剪贴板、CoreAudio重采样、主线程`winit` Viewer与Retina `dfy-manager`、`Dafeiyu Remote.app`及PKG/DMG | Apple Silicon macOS 27.2.1实测VideoToolbox 5/5、Clipboard 29+1+1、Audio 12/12、Capture枚举2940×1912、Input 25+7+1、Manager 11/11、CLI 46/46、实包PKG/DMG构建挂载与`selftest`通过（E20）；无TCC回环优雅报错不崩溃 | 授Screen Recording / Accessibility TCC后的真实屏幕像素抓取、真实键鼠注入/热键、GUI交互目视、Apple ID签名公证、Mac↔Windows物理机互控 |
| Android主控与被控 | 004/015/020 | Kotlin前台服务、MediaProjection、VirtualDisplay、MediaCodec与Rust JNI/Quinn宿主；主控和被控原生角色已建立；Host/Viewer统一原生主题、卡片、分区、状态、表单与会话操作；控制器质量反馈、宿主运行时编码器重建、超时/旧线程/旧投影回调/跨主机代际隔离已接线；无活动Viewer时外部帧入口拒绝入队，但尚未停止MediaCodec编码；v0.2.1 Release APK/AAB接入仓库外专用keystore，CI签名凭据缺失即失败 | 历史最终Debug APK（SHA-256 `f5aad85a24fff0a12a7ca2b24e14441b5e435da66c3eb0c912d5bba02fa815de`）已在API36模拟器完成Rust 17、Kotlin/Android 34回归、真实Windows画面显示、非黑像素、Apply/Reset与硬件Back验收；新UI Debug APK（SHA-256 `95c2d382d2dad92ae69b0559583e453fbee66f4de1c6d760c49b3efe5e657c36`）完成35测试、lint 0错误/25警告及构建；v0.2.1本地签名APK/AAB通过apksigner/jarsigner、证书、版本、ABI与ZIP载荷检查，35测试0失败、lint 0错误/19警告；v0.2.1+ci2单入口修复候选删除第二个LAUNCHER并在主界面提供查看端入口，同时修复主界面启动崩溃；API36模拟器旧版覆盖安装与全新安装均仅1个启动项，主界面→查看端跳转通过，36测试0失败、lint 0错误/19警告，见STATUS/E22 | 新UI未取得设备目视验收；历史测试桥不是产品网络路径；Android发送侧首帧/编码启动延迟仍待优化；主控真机/输入、被控真机/跨机、签名包真机安装/升级、设备端反复热调整、独立多虚拟屏/无头、文件/剪贴板/输入互操作、厂商限制；空闲编码器暂停/恢复与能耗未验收；持续120fps/12Mbps、弱网、进程死亡授权恢复、预测性Back手势仍未验收 |
| 虚拟显示统一核心接口 | 006/027 | 独立dfy-vdisplay、spec、能力位、后端capture endpoint与错误模型；CLI管理及Agent生命周期接线 | 接口/规格测试；Linux真实Xvfb后端测试E06 | Windows/macOS/Android统一验收、会话级所有权、权限与后台恢复 |
| Windows虚拟显示 | 006/027 | 用户态驱动枚举/IOCTL；另有驱动工程 | 实现代码/协议测试，不是驱动装机验收E06 | 驱动回调/像素获取/EDID、WDK签名、HLK/WHQL、DXGI端点、无头、多屏、热改模式、故障恢复 |
| Linux虚拟显示 | 006/027 | 独立Xvfb后端、PID/start-time/cmdline+flock所有权、几何验证、调整/恢复；Agent/采集/输入按选定端点接线；活动采集故障自动重建并请求关键帧，输入按共享最新端点重连；Agent新增5s后台健康检查任务；DPI 以 `-dpi` 传入 Xvfb | Debian QEMU真实后端测试、Release及真实DEB安装后Agent→View 640×480链路均通过；当前Release活动终止Xvfb后188/188解码/0失败，换端点真实X11输入重连读回(123,234)；无Viewer连接时后台任务在8s内重建被杀Xvfb；`xdpyinfo`读回120DPI；xmessage真实窗口令两帧相差40,181像素，见STATUS | 网络输入、跨机、动态刷新率、Wayland/DRM扩展显示、复杂GUI应用 |
| macOS虚拟显示 | 006/027 | 明确拒绝UnsupportedBackend；`CGVirtualDisplay`为未公开私有API，不适宜公证分发，保持诚实报错边界 | 实现边界E06/E20 | 若未来Apple提供受支持公开虚拟显示API后再行评估 |
| Android虚拟显示 | 006/027 | MediaProjection + 单`VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR`；不是独立无头显示 | API36模拟器镜像记录 | 独立多屏、无物理显示器、自定义模式、生命周期/恢复、权限与厂商兼容 |
| 多屏/横竖屏/1080p–4K/刷新率 | 006/009/017 | 采集枚举/选择与虚拟屏规格部分存在 | 源码、测试定义；未完整验收 | 多虚拟屏独立控制、不扰动物理屏、动态分辨率与刷新率 |
| Windows键鼠和断线释放 | 007 | SendInput、HID/VK/扫描码、PressedState、release_all；控制端低级钩子＋5ms光标/按键状态轮询，共用坐标/按下集合去重，停止热键前显式释放 | 实现代码；历史本机16/16报告E01；Windows 43＋dfy-input 18测试通过；Windows→Linux网络指针6条成功，见STATUS | 本环境SendInput合成键钮不进入钩子/Raw Input/跨进程持续按键高位，Windows采集端真实键钮、组合键/断线/焦点/权限仍待交互桌面验收 |
| Linux X11键鼠 | 007 | 本轮XTest/XKB后端，释放跟踪，明确拒绝Wayland/Text直注；虚拟显示输入连接失败后按最新端点重连并重试一次；控制端XInput2优先、核心X11事件降级并去重 | 实现代码E04；Debian guest真实注入/断线释放及Xvfb换端点重连通过；Windows→Linux Agent网络指针6条成功并独立读回`(419,309)`；Linux主控隔离Xvfb真实指针/左键/A键/上下滚轮/停止热键1测试通过，见STATUS | Linux主控真实桌面XInput2、不同布局/IME、权限失败、真实应用焦点、Wayland；Windows主控合成键盘/按钮在本环境不可观察 |
| 跨OS快捷键/布局/映射配置 | 007/017 | HID映射与快捷键四态决策 | 单测定义；不是完整用户配置E04 | Ctrl↔Command等实际跨OS测试、国际布局、IME、创建/修改/导入/导出UI |
| 剪贴板格式与分块同步 | 008 | Win32、HTML/DIB/PNG/文件列表辅助层；拉取式状态机 | 实现代码；mock QUIC测试定义E05 | 所有格式真实跨OS双向、冲突、大小/分块/哈希/撤权负测 |
| Linux X11剪贴板 | 008 | CLIPBOARD/XFixes/TARGETS/INCR，文本/HTML/PNG；文件列表/RTF/DIB等未实现 | Windows回归及Debian guest真实Xvfb往返/所有者生命周期＋QUIC集成通过E05；Windows↔Linux同机QEMU双向Unicode各36B通过，见STATUS | 真实桌面应用互操作、两台独立设备、权限边界；后端丢弃不持久 |
| 每连接剪贴板授权与方向 | 008/016/017 | 后端要求调用者批准；CLI按显式--allow-clipboard和会话权限执行；首帧不自动上传、每块前置限长、最终BLAKE3校验、未知hash丢弃 | Windows单测/mock QUIC回归通过E05；Windows↔Linux同机QEMU双向Unicode文本各36B与45B分别通过；强制路径迁移后双向35B剪贴板继续同步，见STATUS | 拒绝时不枚举/哈希/读取/上传的端到端负测、逐连接方向/手动自动/通知/撤权；两台独立设备真实应用互操作 |
| H.264硬件优先/软件回退 | 009 | Windows Media Foundation 与 macOS Apple VideoToolbox（`VTCompressionSession`/`VTDecompressionSession`）编码/解码与NV12转换 | 实现代码E07/E20；Windows历史本机E01；macOS Apple H.264 HW/SW 1280×720 30帧实编实解5/5通过 | 各GPU/驱动、回退失败、长稳延迟与Mac↔Windows跨机实流 |
| 音频采集与播放 | 012 | `dfy-proto`音频DATAGRAM通道与16kHz/单声道/20ms s16le帧；`dfy-audio`用cpal提供WASAPI/ALSA/CoreAudio采集与播放后端及Null降级；新增连续线性重采样与多声道上/下混（48kHz立体声↔16kHz单声道）；`--audio-out/--audio-in`已接入agent/view，权限位`PERM_AUDIO_OUT`；100ms抖动缓冲按seq重排/计丢包并可自适应，迁移时重置序号并丢弃旧纪元包；`--volume/--mute`软件增益 | 协议22+1、dfy-audio macOS CoreAudio `device-backend` 12/12、CLI 46单测通过；跨OS直连448包/447帧（50%静音）、强制迁移新纪元572包/569帧/丢包0，见STATUS与E20 | 真实麦克风采集端到端听感（CoreAudio仅采麦克风非系统内录）；迁移握手期间真实音频间隙；无丢补/回声消除；Linux/macOS跨机 |
| H.265与AV1 | 009 | HEVC仅探测/枚举入口，非完整编码器；AV1未见实现 | 未实现完整链路E07 | 能力协商、编码/解码、支持时启用与不支持时回退 |
| JPEG跨平台基础串流 | 009 | 软件JPEG/MJPEG后端 | 实现代码/测试定义E07 | Linux真实性能、帧率/CPU、跨机可用性 |
| 自适应码率/分辨率/帧率 | 009 | 接收端500ms质量反馈、Quinn RTT/丢包、确定性ABR、共享 adaptation state、事务重建编码器；用户fps/码率/分辨率是上限，宽高为保持宿主比例的最大包围盒，fps支持1–120；全零请求恢复启动上限；首帧前发送已确认的初始`VideoConfig`；VBR实测吞吐只限制推荐码率，不再单独触发降级/恢复；Android外部编码器运行时控制及跨主机代际隔离已接线 | Windows真实WGC/H.264/QUIC 55秒受控排队验收：473重组/473解码/0解析失败、84反馈窗口，2944×1856/120fps逐级降至956×602/40fps，再恢复至1488×938/63fps；7次设置/7次重建/0错误；loopback恢复用户上限及关键帧回归通过；CLI已接编码器重建工厂。Android UI经认证控制请求320×200/30fps/2000kbps，宿主确认316×198/30fps/500kbps，Reset确认2940×1852/120fps/500kbps；两端各2次成功/0错误，见STATUS；Android Rust/Kotlin构建与竞态回归通过 | 恢复到120fps上限的长时复测、真实弱网推荐质量、Android设备端反复重建实测、H.264/MJPEG各路径与长稳 |
| 区域更新与高效采集 | 009 | DXGI相关逻辑；Linux X11当前全帧GetImage | 局部实现E03 | Linux增量/共享内存等效率路径、端到端区域更新、性能量化 |
| 质量档位与8项连接统计 | 009/017 | 窗口渲染统计/标题；`dfy-ui`的`ConnectionStats`与`title_with_connection`展示分辨率/FPS/码率/RTT/丢包/解码与渲染耗时/路径/Relay并由`dfy-cli`每500ms推送；View路径采样驱动切换；候选路径按`RequestProbe`发轻量QUIC探测；中文统计卡支持长文本省略 | `dfy-ui` 78+3测试通过；Windows统计卡真实显示并通过；弱网自动切换与64B/1200B MTU丢包探测对照实测见STATUS及`notes/nonblocking-migration-probe-20261127.md` | 四档策略、真实Windows交互桌面标题栏目视与真实链路统计复核 |

## 网络、中继与安全

| 能力 | 需求模块 | 阅读到的实现状态 | 当前证据范围 | 尚缺的验收 |
| --- | --- | --- | --- | --- |
| P2P/STUN/UDP打洞 | 010 | STUN解析、候选、打洞与CLI桥 | 实现代码；mock/loopback测试定义E09 | 不同NAT/IPv4/IPv6/真实公网、多候选与失败诊断 |
| 用户手动指定Relay | 010/013/028 | CLI地址/密钥参数、UDP/TCP桥；新增`--relay-profile <ID|@current>` 从档案读取地址/密钥并打印当前服务器 | 实现代码；历史本机Relay报告E01；本机真实relay档案2.86ms与失败目标负测，见STATUS | 图形客户端选择界面；任意公网用户服务器与外部TLS |
| P2P失败自动回退 | 010/020/026 | `agent/view --auto-path` 已同时建立直连与中继端点，View 用 `PathMonitor` 采样并触发 `SwitchTo`，Agent 用共享采集/会话代际接受新连接 | dfy-path 决策测试；本机真实拥塞代理在无强制开关下自动直连→中继迁移通过，见STATUS/E19 | 两台独立设备、公网 NAT；在途鼠标坐标恢复；拥塞窗口内的探测口径对照未做 |
| 质量驱动可恢复会话迁移 | 014/026 | 新路径完整auth v2；签名PREPARE/COMMIT、权限只减、epoch/token轮换、精确COMMIT幂等；跨连接复用采集/编码/帧序号并仅当前代际发视频；CLI已接入View侧resume握手与帧进度；迁移后握手超时从5s放宽到15s | 真实QUIC resume安全测试、session迁移回归与本机强制双向/拥塞自动迁移全通过：epoch=1、恢复确认=true、旧会话migrated、帧连续；Windows↔Linux跨OS强制迁移后双向剪贴板与文件哈希一致，见STATUS/E19及`notes/crossos-migration-clipboard-20261127.md` | 30秒窗口后的边界、鼠标绝对坐标在途恢复、公网验收；Linux→Windows按住键重放见STATUS |
| 任意数量Relay配置档案 | 013/028 | `dfy-relay-profiles` crate：id/name/host/port/key/region/priority/preferred/tls/health 全字段；无限档案；CLI增删改查、当前服务器、导入合并/覆盖、脱敏/含密钥导出；0600与.gitignore | 44+4测试通过；本机真实relay add/list/use/select/export/import全流程，见STATUS | 图形管理界面；DNS多地址轮询、ACME/外层TLS；Windows ACL细化 |
| Relay池健康/自动选优/故障切换 | 014/026/028 | 四种选择策略：manual / lowest_latency / preferred_first / auto_failover；配对RTT + 8×1200B数据面探针真实填充丢包/带宽并写回；失败状态写回；`next_after_failure` | 策略/健康持久化单测与本机真实可达/不可达探针通过；本机实测配对2.31ms/0ppm/3.84Mbps，见STATUS与`notes/relay-profiles-acceptance-20261126.md` | 带宽是小突发观测值而非容量标定；真实跨服务器迁移仍未实现E11；基础图形面板见本表“商业级主界面”行 |
| 独立Linux Relay服务 | 011 | 独立binary、UDP/TCP转发、配置与状态管理 | 实现代码E12；本轮执行见STATUS | 真实Linux安装版/公网、并发、资源、长稳、ARM64可行性 |
| Relay鉴权/重放/槽位代际 | 011/016 | HMAC/时间窗/nonce/限流；严格过期后判重，满载拒绝而非淘汰；TCP/UDP kind/id/地址绑定，替换中止旧reader/writer | 41 unit+2旧集成+6新真实loopback安全测试通过E12 | 服务重启epoch、真实公网并发、跨承载与资源压力 |
| Relay健康/日志/监控/限流 | 011 | health/metrics/status、速率桶、配置限制 | 实现代码E12 | Linux运行指标、全局/租户限流压力、日志隐私/保留 |
| 一键部署、systemd、Compose | 012/028 | 脚本/Compose和部署说明存在 | 安装资产E13，不等于实装通过 | 官方Linux新机安装、配置、启动、端口、升级、卸载、回滚；字段一致性；Xvfb虚拟显示依赖 |
| 管理面安全与外层TLS | 011/012/016 | Relay启动路径（含直接Config）强制管理监听loopback；原TCP外层未TLS | 运行时loopback修补与测试E12 | 管理面鉴权/资源限制；适用TLS/证书策略及测试 |
| E2E、双向认证、通道绑定 | 016 | QUIC/TLS + Argon2id auth v2 + 角色分离HMAC；transcript绑定Hello/nonce/salt/设备公钥/exporter，客户端先验server proof，无legacy降级 | 16 unit+11真实QUIC安全测试+1 doctest通过；覆盖恶意双TLS中继、跨exporter、反射、错口令、pin、KDF取消配额 | 持久设备信任/配对撤销、完整凭据体系；TCG debug握手性能边界见STATUS |
| 持久设备身份与配对/撤销 | 016 | 密钥生成/公钥字段不等于持钥认证 | 未完整实现；审计E10 | 签名/mTLS等持钥证明、可信配对、撤销、设备目录与过期 |
| 权限、只读、会话终止/审计 | 016/017 | CLI无默认口令、回环监听、输入/剪贴板默认关闭且显式授权；ServerAuth默认VIEW，权限位图按能力构建；Bye立即撤销 | 单元/QUIC权限测试及CLI默认值测试通过 | 逐连接用户批准UI、撤销/终止、完整审计、资源耗尽与攻击面验收 |
| 凭据安全存储与安全更新 | 016/022 | CLI支持隐藏终端输入/有界stdin，Zeroizing存储、Debug去敏、无默认口令；设备身份/relay_key持久安全存储未完成 | credentials单测通过；未落盘 | DPAPI/Keychain/Secret Service或受控secret provider、ACL、签名/回滚防护 |
| 可选2FA/访问控制 | 016 | 未见完整用户流程 | 未实现/未测 | 设计、密钥恢复、策略与负测；不能用口令证明代替2FA |

## UI、扩展和发布

| 能力 | 需求模块 | 阅读到的实现状态 | 当前证据范围 | 尚缺的验收 |
| --- | --- | --- | --- | --- |
| 桌面Viewer、全屏、缩放、输入 | 017 | 原生`winit`/`softbuffer`；DPI缩放、letterbox、帧邮箱、输入隔离；中文圆角悬浮工具条含全屏、暂停/继续、连接统计、断开，支持自动隐藏与键盘导航；内嵌OFL中文字体原生抗锯齿栅格 | Windows宽窗口与706×428窄窗口真实起窗，统计卡、暂停/继续、快捷键及按钮点击通过；`dfy-ui` 78测试+3 doctest、strict Clippy通过，见STATUS/E21 | 高DPI屏幕、macOS/Linux新UI目视、多屏/真实跨机控制与性能；统计默认显示演示数据，不等于真实链路延迟 |
| 商业级主界面和会话工具 | 017 | `dfy-manager`改为`eframe`/`egui`原生窗口；深蓝/薄荷视觉，设备、Agent、中继、显示器、诊断五页；统一卡片/状态/空态/加载/错误/进度与校验；680px以上响应式导航；口令仅stdin且不写入设备书签 | Windows 1122×752与682×752五页及添加设备校验弹窗目视通过；8测试与strict Clippy通过；Linux包与Release流程补齐Manager；macOS bundle显示品牌“大肥鱼远程”，见STATUS/E21 | 设备发现/在线状态、设置、键映射、剪贴板/文件/音频/权限UI；macOS/Linux新UI目视；虚拟显示创建/已填充状态因本机无后端未测 |
| 1分钟Relay配置体验 | 013/017 | Manager中继页使用档案与策略，不要求展示或复制原始密钥；仍保留CLI参数 | Windows Manager页面目视通过，见E21 | 新用户计时、真实中继连接与失败诊断、导入导出交互 |
| 文件传输与目录/断点/限流 | 018/020 | sender/receiver/path/目录计划、ACK/偏移/大小防护 | 集成测试定义E14；历史本机3MiB哈希报告E01；Windows→Linux同机QEMU真实51B文件offer/块/确认/完成及SHA-256一致；Linux→Windows同机QEMU真实50B文件offer/块/确认/完成及SHA-256一致，见STATUS | 两台独立设备、双向大文件/目录/断点/限流、迁移中恢复、UI授权与覆盖确认 |
| 音频 | 009/018/020 | 未见完整audio模块链路 | 未实现/未测 | 原生采集/播放/设备/权限、单/双向、AV同步 |
| 其余20项扩展评估 | 018 | 原文完整保留，未按需求全部实现 | 未完整实现 | 对每项记录必要性、成本、隐私许可与阶段；不阻塞核心 |
| 稳定17模块架构 | 019 | 14个Rust crate承载若干模块；部分边界未落地 | 源码组织，不是全部17能力完成 | Discovery/Auth/Audio/Android/Server Mgmt等边界与稳定API |
| 规定平台×路径×场景测试 | 021 | 单元/loopback/少量历史同机；覆盖率未完整给出 | 测试定义/历史证据 | 9组指定方向×P2P/Relay×16场景，记录真实环境与覆盖率，不把无环境记通过 |
| 开源工程与CI/CD | 022/023 | 许可证、协议/设计、CI/release workflow与CycloneDX SBOM/许可审计(`packaging/generate_sbom.py`)已接线；公开Release只整理安装包并生成中文直链说明，源码/脚本/文档留在仓库 | 当前锁文件`generate_sbom.py --check`通过（18 workspace + 598依赖，含epaint字体OFL/UFL许可）；fmt/UI包check/test/strict Clippy、`smoke.py`、需求台账通过；首次Release CI定位并修复Linux Manager的`rfd/tokio`缺失，修复后的run `46121415301`全部14个job成功；安装包整理/直链生成已本地模拟为恰好10个安装包和10个下载链接，见STATUS/E22 | 真实新Release CI与公开直链下载验收；可复现构建、安全策略/双语文档全量复核；本机未实际构建MSRV 1.75 |
| 各平台可安装Release | 022/024 | 全端打包脚本存在；Windows Inno支持`dfy-manager.exe`；Linux DEB/RPM安装Manager且desktop entry无终端启动；macOS支持`Dafeiyu Remote.app`与PKG/DMG，可见显示名和DMG卷名为“大肥鱼远程”；Release workflow补齐macOS/Linux Manager；Android Release APK/AAB强制签名并验证；公开Release仅附10个安装包 | Linux历史x86_64 DEB/RPM真实构建及systemd生命周期通过；Windows历史0.1.0/0.1.1安装包真实构建；macOS历史arm64实包验包通过（E20）；本轮host-runnable打包mock/静态检查通过；`v0.2.1+ci1`公开Release的14个CI job成功，Android签名APK/AAB已下载复核证书、版本、ABI、ZIP与SHA-256（E22）；安装包整理与直链说明本地模拟通过，见STATUS | 新Release真实CI与公开直链下载；代码签名/Apple公证、Windows交互覆盖安装、macOS系统Applications新UI实装与x86_64实包、Linux新Manager包/aarch64真实安装、Android签名包真机安装/升级与Google Play发布 |

## 证据入口

下面的“测试”链接证明测试代码存在。只有STATUS中记录本轮命令和结果后，才升级为“本轮通过”；源码注释里写了“implemented”也仍须运行验收。

| ID | 证据 | 解释 |
| --- | --- | --- |
| E01 | [历史真实链路记录](../design/014-real-pipeline-verified.md)、[旧项目说明](../README.md) | 历史Windows同机实验。报告过6秒DXGI→MF→QUIC→解码：398 datagrams、401.57KiB、60组装帧、24解码帧、0解析错；不是本轮重测或公网性能 |
| E02 | [会话测试](../crates/dfy-session/tests/e2e.rs)、[会话实现](../crates/dfy-session/src/agent.rs) | loopback帧传递、自动质量反馈、编码器降级/恢复及共享采集跨连接；不证明真实双路径网络迁移 |
| E03 | [Linux采集](../crates/dfy-capture/src/linux.rs)、[采集平台工厂](../crates/dfy-capture/src/lib.rs) | X11 GetImage和显示枚举；全帧、非原生Wayland完整桌面 |
| E04 | [输入入口与平台边界](../crates/dfy-input/src/lib.rs)、[事件/释放状态](../crates/dfy-input/src/event.rs)、[输入测试目录](../crates/dfy-input/tests/) | Windows SendInput与本轮Linux X11后端；物理事件/IME/国际布局须分别验证 |
| E05 | [剪贴板接口](../crates/dfy-clipboard/src/lib.rs)、[同步状态机](../crates/dfy-clipboard/src/sync.rs)、[mock QUIC测试](../crates/dfy-clipboard/tests/clipboard_quic.rs) | 后端与传输/授权分离；mock不是原生系统剪贴板 |
| E06 | [虚拟显示入口](../crates/dfy-vdisplay/src/lib.rs)、[Linux Xvfb后端](../crates/dfy-vdisplay/src/linux_xvfb.rs)、[Xvfb集成测试](../crates/dfy-vdisplay/tests/xvfb_integration.rs)、[Windows用户态](../crates/dfy-vdisplay/src/win_driver.rs)、[驱动工程](../crates/dfy-vdisplay/driver/) | Linux有真实后端生命周期测试及Release/已安装DEB的Agent→View像素来源/清理验收；Windows仅用户态/骨架，macOS/Android独立多屏未实现。均不说明全平台已装机验收 |
| E07 | [编码器接口/实现选择](../crates/dfy-codec/src/lib.rs)、[Windows MF](../crates/dfy-codec/src/mf.rs)、[Windows WGC](../crates/dfy-capture/src/wgc.rs)、[JPEG](../crates/dfy-codec/src/jpeg.rs)、[采集控制](../crates/dfy-session/src/capture_loop.rs) | HEVC探测、set_bitrate方法和enum不能替代完整编码器；自适应闭环另由接收反馈和事务重建证据支持；WGC真实链路结果以STATUS为准 |
| E08 | [窗口应用](../crates/dfy-ui/src/app.rs)、[渲染统计](../crates/dfy-ui/src/stats.rs)、[帧逻辑](../crates/dfy-ui/src/frame.rs) | 软件渲染Viewer及纯逻辑；不是完整产品主界面 |
| E09 | [P2P loopback测试](../crates/dfy-p2p/tests/p2p_loopback.rs)、[P2P实现](../crates/dfy-p2p/src/) | mock STUN、超时、重传、打洞/传包；不代表真实对称NAT/公网穿透 |
| E10 | [修补前安全审计](../notes/security-audit-current.md) | 只读分析，非已执行攻击证明；后续修补结果另记，不能自动认定仍有漏洞或已经修复 |
| E11 | [路径策略](../crates/dfy-path/src/)、[迁移协议](../crates/dfy-transport/src/resume.rs)、[路径设计](../design/028-path-migration.md) | 纯策略状态机不做IO；安全迁移与共享运行时已有loopback，真实P2P↔Relay自动切换仍未完成 |
| E12 | [Relay配置](../crates/dfy-relay/src/config.rs)、[Relay状态](../crates/dfy-relay/src/state.rs)、[Relay源码](../crates/dfy-relay/src/) | 独立服务实现；安全补丁及真实Linux部署须单独验证 |
| E13 | [安装脚本](../deploy/install-relay.sh)、[Compose](../deploy/docker-compose.yml)、[部署说明](../deploy/README.md) | 资产存在；旧示例字段/安全说明与代码有不一致，见STATUS |
| E14 | [文件传输集成测试](../crates/dfy-files/tests/e2e_transfer.rs)、[文件模块](../crates/dfy-files/src/) | 包含目录、断点、危险路径/大小/乱序等定义；未替代真实迁移与UI验收 |
| E15 | [CI](../.github/workflows/ci.yml)、[Release workflow](../.github/workflows/release.yml)、[许可证](../LICENSE) | 定义/资产不等于最新运行成功或全平台产物可安装 |
| E16 | [Windows输入采集](../crates/dfy-cli/src/input_capture.rs)、[Linux控制端映射测试](../crates/dfy-cli/src/input_capture.rs) | Windows低级钩子＋5ms状态轮询，Linux XInput2优先/核心X11降级；隔离Xvfb真实指针/按钮/A键/滚轮/停止热键通过，物理桌面焦点/IME/多屏未验 |
| E17 | [跨OS验收记录](../notes/crossplatform-acceptance-20261126.md) | Windows↔Linux同机隔离VM双向视频、指针、键盘、双向Unicode剪贴板与文件哈希；不是两台物理设备或公网/Relay验收 |
| E18 | [用户中继池](../crates/dfy-relay-profiles/src/lib.rs)、[中继池命令](../crates/dfy-cli/src/relay_profiles_cmd.rs)、[验收记录](../notes/relay-profiles-acceptance-20261126.md) | CLI档案CRUD、四种选择策略、健康探针、脱敏导入导出与本机真实relay测试；GUI仍未完成 |
| E19 | [可恢复路径迁移验收](../notes/auto-path-acceptance-20261126.md)、[CLI接线](../crates/dfy-cli/src/real.rs) | 本机真实直连→中继强制迁移：epoch递增、COMMIT确认、旧会话migrated、帧#25连续；诊断强制触发，不等价于真实弱网自动触发或回切 |
| E20 | [macOS原生与PKG/DMG验收](../notes/macos-native-20261128.md)、[VideoToolbox](../crates/dfy-codec/src/vt.rs)、[ScreenCaptureKit](../crates/dfy-capture/src/macos.rs)、[CGEvent输入](../crates/dfy-input/src/macos.rs)、[NSPasteboard](../crates/dfy-clipboard/src/macos.rs)、[macOS打包脚本](../packaging/macos/build-installer.sh) | Apple Silicon macOS 27.2.1实机模块测试、CoreAudio重采样、VideoToolbox H.264实编实解与`Dafeiyu Remote.app` PKG/DMG真机构建挂载校验；TCC交互桌面与Mac↔Windows跨机仍待实测 |
| E21 | [全平台UI打磨记录](../notes/ui-all-platforms-20261128.md)、[Manager](../crates/dfy-manager/src/main.rs)、[Viewer工具条](../crates/dfy-ui/src/toolbar.rs)、[中文字形渲染](../crates/dfy-ui/src/overlay_text.rs)、[Android主题](../android/app/src/main/java/remote/dafeiyu/UiTheme.kt) | Windows Manager/Viewer宽窄窗口目视与交互、UI包8+78+3测试和strict Clippy、Android35测试/lint/Debug构建、598依赖SBOM与host-runnable打包检查；不含macOS/Linux新UI目视、Android新UI设备目视、高DPI或真实跨机验收 |
| E22 | [Android签名发布记录](../notes/android-signing-release-20261128.md)、[Android打包脚本](../packaging/android/build.sh)、[签名初始化脚本](../packaging/android/signing.gradle)、[Release workflow](../.github/workflows/release.yml) | `v0.2.1+ci1`公开Release与run `46121415301`；14个CI job成功；公开签名APK/AAB/Debug APK已下载并复核SHA-256、APK v2证书指纹、AAB签名、版本/SDK/ABI/ZIP载荷；首次CI的Linux Manager运行时特性缺失已修复；私钥与凭据在仓库外，CI缺少签名凭据即失败；`v0.2.1+ci2`本地候选已修复双LAUNCHER与主界面启动崩溃，并在API36模拟器完成旧版覆盖安装、全新安装和查看端跳转验证；公开Release流程已改为只附10个安装包并生成直链说明，介绍页已用检查后的当前截图重写；不等于ci2已发布、物理真机安装/升级、Google Play或完整功能验收 |

本表按代码实际能力保守拆分。任何更新都应同时写入命令/环境/结果，并保留未测维度；不要把“本次修好了一个负测”升级为整类安全或平台功能完成。
