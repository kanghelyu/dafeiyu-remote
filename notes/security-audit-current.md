# 当前安全审计（2026-11-25，修补前快照）

本轮仅审计并记录；没有修改源码、提交或推送，没有启动真实 Agent/View，也没有运行真实桌面采集、输入注入或系统剪贴板测试。发现基于逐行阅读及调用链分析，下面的攻击步骤为可复现方法，**不标作已经执行的攻击验证**。初始工作区已有 CLI `real.rs` 的并发修改；行号对应本轮读取版本，后续修补可能移动。

需求基线：[业主要求](<D:/agent working/dafeiyu/notes/000-owner-brief.md#L45>) §4.3、§6、§8、§17：E2E、双向设备认证、中继不得解密、凭据不得明文落盘、恢复防劫持和重放、每连接剪贴板授权、P2P↔Relay 安全迁移。

## 结论与优先顺序

目前的 QUIC 加密可保护被动窃听，但整体实现不满足生产级端点身份与授权要求。应先修复 A01–A05，随后补齐 A06–A10。已有防护包括随机握手 nonce、HMAC 常量时间验证、可选 SPKI pin、控制帧长度限制、中继 bind HMAC/时间窗、有限会话数与速率桶；这些不能替代互认证、配对和权限执行。

| 编号 | 优先级 | 问题与实际影响 |
|---|---|---|
| A01 | P0 | 真实 Agent 对全网卡开放、默认公开口令且默认控制/剪贴板许可，可被可达攻击者直接接入 |
| A02 | P0 | 无 pin 时接受任意证书；密码证明不绑定 TLS 连接，恶意中继可以两条 TLS 实时转发认证并读改会话 |
| A03 | P1 | 密码派生明文 relay tag，向中继和链路观察者提供快速离线字典验证器 |
| A04 | P1 | 剪贴板没有逐连接用户批准，权限位图仅宣告，未用于业务层强制检查 |
| A05 | P1 | 口令明文输出；设备 seed 和 relay 密钥明文落盘，密钥生成无私钥访问控制 |
| A06 | P1 | nonce 去重表容量淘汰使时间窗内重放可再次通过；重启也清空重放记录 |
| A07 | P1 | TCP 允许覆盖热槽位；被替换旧 TCP 连接继续按 tag/role 转发，没有代际 ID 检查 |
| A08 | P1 | SessionResume 没有原会话/设备/权限/时效证明，只是关键帧请求，不能宣称安全迁移 |
| A09 | P2 | 管理面回环限制只在 config check 中执行，实际启动接受任意监听地址 |
| A10 | P2 | Agent 串行等待网络握手与整场会话，无登录尝试限流；未认证客户端可占用接入队列 |

## A01：默认口令和默认远控权限

证据：[CLI 参数](<D:/agent working/dafeiyu/crates/dfy-cli/src/main.rs#L85>) 87–109 行：host 默认 `0.0.0.0`，password 默认 `test`，no_input/no_clipboard 默认 false；[参数转换](<D:/agent working/dafeiyu/crates/dfy-cli/src/main.rs#L245>) 252–253 行把二者取反。真实 Agent 在[握手后](<D:/agent working/dafeiyu/crates/dfy-cli/src/real.rs#L248>) 248–300 行直接开启采集及业务处理器。[默认权限](<D:/agent working/dafeiyu/crates/dfy-proto/src/consts.rs#L78>) 为 VIEW|CONTROL|CLIPBOARD。

攻击前提：用户运行默认真实 Agent，操作系统采集/注入/剪贴板后端可用，攻击者可达 UDP 21116，且当前没有占用该单会话服务。攻击者以默认口令通过正常握手即可收到桌面，发送输入事件或剪贴板消息；不需本地账户或设备配对。安全复现应使用 fake capture、记录式 input sink、内存 clipboard，不能直接在用户桌面执行。

最小修补：删除真实模式固定口令默认值，要求安全交互输入或随机高熵首次配对凭据；默认只读、剪贴板关闭；权限由本端逐连接授权决定。诊断 demo 若保留弱默认值应仅绑定 loopback 并明确隔离。

## A02：TLS 分拆实时中间人、无双向设备认证

证据：[服务端 TLS](<D:/agent working/dafeiyu/crates/dfy-transport/src/tls.rs#L132>) 135–136 行为 `with_no_client_auth()`；[客户端 TLS](<D:/agent working/dafeiyu/crates/dfy-transport/src/tls.rs#L178>) 180–191、230–235 行未给 expect 时接受任意合法 DER 证书。当前仅打印 observed 指纹，没有保存并在下次连接比较，所以不是真正具有持久绑定的 TOFU。[绑定 Agent](<D:/agent working/dafeiyu/crates/dfy-transport/src/endpoint.rs#L15>) 17 行每次启动生成新身份。

[应用握手](<D:/agent working/dafeiyu/crates/dfy-transport/src/handshake.rs#L84>) 84–99 行仅记录 device_pubkey 长度，未验证设备持钥；120 行只检查 nonce 的口令 HMAC；客户端在 211–248 行收任意 Challenge 后提交证明并相信 Welcome，没有服务端口令证明。[真实 View](<D:/agent working/dafeiyu/crates/dfy-cli/src/real.rs#L462>) 462 行发送全零公钥。[证明算法](<D:/agent working/dafeiyu/crates/dfy-transport/src/util.rs#L45>) 48–69 行：`HMAC-SHA256(SHA256(password), nonce)`，没有 TLS exporter、双方角色、Hello 身份声明或原会话绑定。

攻击前提：View 没有预先经可信通道固定正确 SPKI；攻击者控制所选 relay、路由/解析或可作主动链路中间人。攻击者用自己证书终结 View→M TLS，同时作为客户端建立 M→Agent TLS，把 Agent 的 Challenge 原样给 View，再把 View 的 Auth 原样给 Agent。两侧 TLS 均有效且 HMAC 相同，攻击者不需要事先知道密码，即可在自己掌握的两条 TLS 中读取/修改视频、输入、剪贴板及文件。恶意假 Agent 还可自行发 nonce 收集证明后离线猜密码。正确 pin 的连接可阻止这类证书替换；被动 relay 在正常转发模式不能解密 QUIC 密文。

最小修补：握手证明必须绑定标准 TLS exporter、双方随机 nonce、角色及规范编码 transcript；双方验证密码证明，固定有界 Argon2id 参数提高弱口令离线成本，无旧模式静默回退。不要将此称为 PAKE。生产最终方案仍需持久设备密钥、OS 安全存储、带外配对、可信设备撤销和设备持钥签名/mTLS；口令互证不能代替每设备身份。保留且测试 pin。

## A03：relay tag 泄露口令离线验证器

证据：[默认 tag 派生](<D:/agent working/dafeiyu/crates/dfy-cli/src/relay_bridge.rs#L75>) 76–84 行为 `BLAKE3(password || "\0dfy-relay-tag\0" || relay_addr.to_string())[:16]`；[Bind 线格式](<D:/agent working/dafeiyu/crates/dfy-relay/src/proto.rs#L67>) 67–75 行把 tag 明文放在 UDP/TCP Bind 中。[CLI 输出](<D:/agent working/dafeiyu/crates/dfy-cli/src/main.rs#L360>) 361–365 行也打印完整 tag。

攻击前提：观察一个使用默认 tag 派生的 Bind，或者拥有 relay 日志/进程/流量访问权，并知道客户端使用的 relay IP:端口字符串。对每个字典口令计算相同 BLAKE3 并比对 16 字节，无需 relay_key，也无需破 TLS。随机高熵密码降低字典成功率，但不消除设计泄露。

最小修补：tag 为独立随机 128bit/以上会合 ID，通过受认证配对通道交换；不得派生自口令或设备秘密。若过渡版要求显式 tag，应拒绝省略并给出随机生成方式；不要用“加盐快速哈希”继续公开口令验证器。

## A04：剪贴板与权限执行缺口

证据：[每连接处理器构建](<D:/agent working/dafeiyu/crates/dfy-cli/src/real.rs#L295>) 295–300 行只继承进程级 allow_clipboard；[桥接](<D:/agent working/dafeiyu/crates/dfy-cli/src/clipboard_bridge.rs#L108>) 120–124 行直接打开后端，192–203 行轮询广播，233–255 行直接接受远端消息。没有本端逐连接批准流程。View 虽需显式 `--clipboard`，对端行为不能代表被控端用户同意。

[同步状态机](<D:/agent working/dafeiyu/crates/dfy-clipboard/src/sync.rs#L194>) 194–258 行初次轮询就读取现有本地数据并发送哈希和格式；319–356 行按 Pull 回传。远端 DataChunk 在 359–398 行不要求先前 Pull、无 hash 内容校验，未知 hash 默认解释为文本；在 last=true 之前没有对累计 buffer 执行 1MiB 上限，可在授权剪贴板连接中持续发 last=false 块耗尽内存。此处后者为代码分析，未执行内存放大试验。

权限位图仅在[握手 Welcome](<D:/agent working/dafeiyu/crates/dfy-transport/src/handshake.rs#L129>)中宣告。会话在[业务转发](<D:/agent working/dafeiyu/crates/dfy-session/src/agent.rs#L209>) 210 行及 261–268 行不检查权限；[注入处理器](<D:/agent working/dafeiyu/crates/dfy-cli/src/input_bridge.rs#L230>)只根据后端是否启用执行。把 ServerAuth.permission_bitmap 设为 VIEW 不能撤销已启用的 handler 能力。

攻击前提：掌握有效密码/会话（例如 A01/A02），Agent 保持默认剪贴板设置。自定义客户端可读取当前剪贴板、覆盖本地文本；不需被控用户对该连接单独批准。

最小修补：逐连接显式批准与方向控制，批准前不枚举/哈希/读取；本地拒绝优先且在 session dispatch 强制执行协商权限；断线清掉授权。分块状态绑定请求/对象 hash/offset，追加前检查累计上限，拒绝未经请求的块并校验最后 hash。

## A05：凭据明文输出与保存

证据：[真实 Agent 输出](<D:/agent working/dafeiyu/crates/dfy-cli/src/real.rs#L217>) 218–220 行打印完整口令；[demo 输出](<D:/agent working/dafeiyu/crates/dfy-cli/src/main.rs#L412>) 413 行相同。命令行 password/relay_key 字符串参数可能进入 shell history、进程参数清单和调用者日志。

[设备密钥生成](<D:/agent working/dafeiyu/crates/dfy-cli/src/genkey.rs#L20>) 24–33、52–56 行把 Ed25519 seed 直接 hex 写入 device.key，使用 File::create，无独立权限、加密或禁止覆盖；该 key 当前没有接入真实握手，故不能声称读取它已经等价于劫持现有会话，但生产接入前必须修复。[relay 模板](<D:/agent working/dafeiyu/crates/dfy-relay/src/config.rs#L323>) 338–360 行保存 base64 明文 relay_key；[config init](<D:/agent working/dafeiyu/crates/dfy-relay/src/main.rs#L243>) 243–251 行先写后 chmod、并再次输出密钥及完整客户端配置，Unix 0600 不等于加密，Windows restrict_permissions 为 no-op（269–272 行）。config check 还在 280–284 行打印密钥部分前缀。

攻击前提：能读取终端/CI日志、进程命令行、备份、或配置/私钥文件 ACL 允许的本地账户。不能将“任意远程攻击者都可读本地文件”作为前提省略。获得 relay_key 可创建其他 relay bind；获得密码可直接远控。

最小修补：移除密码和密钥输出，交互输入/受保护 FD/OS keyring；设备私钥用 DPAPI/Keychain/Secret Service，Linux 无 keyring 的 unattended 部署使用明确受控外部 secret provider/加密封装。安全创建新文件、原子落盘与私有权限须先于写入。最终仍需检查安装与备份流程。

## A06：容量淘汰导致 Bind 重放窗口重新开放

证据：[nonce 缓存](<D:/agent working/dafeiyu/crates/dfy-relay/src/auth.rs#L74>) 74–91 行在检查重复前执行 `expired || self.order.len() >= self.capacity` 淘汰。默认容量[8192](<D:/agent working/dafeiyu/crates/dfy-relay/src/config.rs#L23>)，时间窗 ±300s。鉴权成功后在[state.bind](<D:/agent working/dafeiyu/crates/dfy-relay/src/state.rs#L168>) 168–179 行查询缓存，缓存重启不持久。

纯状态复现：创建 NonceCache(capacity=1,ttl=660s)，同一 Instant 先 insert(tag,role,nonce) 为 true，再次原样 insert 仍为 true：第二次先删除旧项，再发现“不重复”。默认8192时，保存一个有效 Bind，在有效时间窗内先让8192个有效不同 nonce 被处理（正常高并发、能观察足够合法流量，或持共享 relay_key 的其他用户），然后重放已淘汰 Bind。填满时重放最旧项甚至不需额外新包。重启 relay 后，窗口内截获的旧 Bind 同样被视为新请求。

前提：攻击者已观察有效 Bind，且有上述容量/重启条件；不能靠任意未鉴权垃圾污染缓存（代码确实先验 HMAC）。后果为配对端点被重定向/拒绝服务；既有 QUIC 的密钥认证仍限制其直接解密。重放到 TCP 可结合 A07 绕过 UDP 热槽位保护。

最小修补：在过期清理后先判重，容量满则 fail closed/按身份限额，不能移除仍在有效时间窗内的 nonce 并继续接受；引入服务端启动 epoch/challenge 与端点持钥的重绑定票据，以避免重启和被动捕获重放。补满载最旧项重放、跨UDP/TCP及重启测试。

## A07：TCP 覆盖热槽位与旧连接代际失效未执行

证据：[槽位替换](<D:/agent working/dafeiyu/crates/dfy-relay/src/state.rs#L236>) 236–255 行只对新 UDP 且 hot 拒绝；新 TCP 无条件 rebind。[TCP](<D:/agent working/dafeiyu/crates/dfy-relay/src/tcp.rs#L147>) 147–163 行旧连接继续调用 forward(tag,role,...) / touch(tag,role,...)，不传入 bind 分配的 id；id 仅在 183–185 行退出 unbind 时检查。forward 和 touch 在[state](<D:/agent working/dafeiyu/crates/dfy-relay/src/state.rs#L363>) 364–410 行也不验证发送者是否仍为当前 endpoint。

复现：以合法共享 relay_key 对同 tag/role 建 TCP A，再建 TCP B；B 立刻替换槽位。A 不关闭，继续发 CHAN_DATA，仍会向对端转发；A 发 CHAN_PING 会刷新 B 所在槽位。也可让 TCP 直接替换正活跃的 UDP 槽位。前提为有共享 relay_key+tag，或者利用A06重新接受被捕获的 Bind；仅知道 tag 不足以构造新HMAC。影响为租户间抢占、流量截取/注入密文和DoS；不能孤立宣称取得 QUIC 明文。

最小修补：所有承载方式统一执行可证明原设备持钥的 rebind 票据/guard；forward/touch/unbind 都要求当前 endpoint id；替换时主动关闭旧 writer/reader。relay 的共享部署 key 只能授权接入，不能授权接管别人的会合会话。

附带：TCP fallback 在[tcp.rs](<D:/agent working/dafeiyu/crates/dfy-relay/src/tcp.rs#L4>) 8、85–107 行为明文 TCP，无 TLS 协商；默认开启公开21118。内部 QUIC 密文仍受端点TLS保护，但外层 Bind/元数据不保密，不能称 relay TCP TLS 已实现。应禁用默认明文降级或实现并验证外层标准TLS。

## A08：恢复消息不证明原会话归属

证据：[SessionResume](<D:/agent working/dafeiyu/crates/dfy-proto/src/msg.rs#L404>) 409–415 行只有 last_frame_id/last_frame_seq；[Agent处理](<D:/agent working/dafeiyu/crates/dfy-session/src/agent.rs#L193>) 193–206 行不验证原session/device/token/期限/代际，直接请求关键帧并返回Ok。[Client](<D:/agent working/dafeiyu/crates/dfy-session/src/client.rs#L106>) 108 行甚至每次新连接都发送(0,0)，超时仍继续。重新[握手](<D:/agent working/dafeiyu/crates/dfy-transport/src/handshake.rs#L129>)130行生成全新session_id；真实Agent只保留采集资源、重新建立handler，未保留安全会话身份。

当前边界：恢复消息必须位于已经通过server_handshake的QUIC连接内，因此**未发现仅靠已知frame_id即可绕过密码接管的路径**。问题是未实现“原设备/原权限续接”的安全语义，不能把复用采集器与关键帧称为安全会话迁移；任一知道共享密码的新设备均可得到新授权。

最小修补：会话表持有随机session id、已配对设备公钥、原始权限、expiry和单调epoch；恢复票据只在E2E内发放并绑定原设备持钥及新TLS exporter；限时、撤销、单次/单调防重放；新路径成功后原路径失效；剪贴板恢复遵守原授权及本端重新确认策略。建立真实恶意恢复负测后再声称完成。

## A09：管理面实际启动不强制回环

证据：[Config解析](<D:/agent working/dafeiyu/crates/dfy-relay/src/config.rs#L261>)267行接受任何SocketAddr；[启动](<D:/agent working/dafeiyu/crates/dfy-relay/src/relay.rs#L106>)108行直接bind；[仅config check](<D:/agent working/dafeiyu/crates/dfy-relay/src/main.rs#L290>)292–295行才拒绝非回环。[admin服务](<D:/agent working/dafeiyu/crates/dfy-relay/src/admin.rs#L76>)76–101行无鉴权暴露health/metrics/status。

前提：运维配置或自动生成配置把admin_addr设到0.0.0.0/公网地址，并直接serve而不跑check。影响是统计、会话数、监听地址等泄露和无上限spawn资源消耗；当前接口只有读操作，未发现管理凭据/桌面内容通过该接口直接返回。

最小修补：Config::from_file_config及relay::start均强制loopback（或显式鉴权的远程管理模式）；限制并发、请求/响应超时，不要让文档警告成为运行时边界。

## A10：未认证接入可占用串行服务

证据：[真实Agent](<D:/agent working/dafeiyu/crates/dfy-cli/src/real.rs#L226>)226–259行先await incoming，再串行await应用握手，327行串行await整场serve_agent；[应用握手超时](<D:/agent working/dafeiyu/crates/dfy-transport/src/lib.rs#L51>)为10s；认证失败[仅固定100ms](<D:/agent working/dafeiyu/crates/dfy-transport/src/handshake.rs#L120>)124行延迟。没有按源/设备登录限速或失败冷却。引入Argon2id之后还必须有全局KDF并发/队列上限。

前提：网络可达但无需口令。完成TLS后不给完整Hello/Auth，反复消耗应用握手窗口，或停在QUIC incoming建立阶段；合法连接必须排队。已知密码用户也可长期占用唯一serve循环。现有10s应用超时有界但不等于抗资源耗尽。

最小修补：把网络握手放到严格有界并发任务、明确QUIC与应用阶段超时、按来源与全局限流；真实远控权限批准后再占用采集资源。不要无限spawn或让KDF堆积阻塞事件循环。

## 验证范围与下一步

- 本轮已读取transport TLS/endpoint/handshake/util/session，session agent/client，relay auth/state/UDP/TCP/config/proto/lib/admin/main/装配，以及CLI入口/真实链路/剪贴板/输入桥/密钥生成和相关协议结构。
- 未启动真实桌面/输入/剪贴板测试，未发送外部攻击流量，未做完整依赖漏洞扫描或安装升级链签名审计。试图定位cargo/rustc时PATH不可见，返回命令未找到；这不是测试通过的证据。
- 已向lead报告P0及最小修补。后续授权的transport修补应独立记录验证结果，**本报告属于修补前审计，不因后续改动自动变成“问题仍存在”或“已经安全”的结论**。
- 最低发布门槛：无公开默认凭据/无secret输出；真实QUIC恶意relay分拆/不同exporter/错误口令负测；持久身份与配对；逐连接权限强制；恢复重放/劫持负测；relay容量与重绑定代际负测；OS密钥存储实测；所有结果保留平台和测试范围。
