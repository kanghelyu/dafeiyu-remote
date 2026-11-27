# 官方公开资料来源与证据边界

查阅日期：2026-11-25。仅使用厂商/项目官网、官方文档与项目官方仓库；未执行竞品性能基准或授权互操作实验。本轮没有依据本地竞品安装包反编译来确认功能、协议或安全机制。历史逆向分析仅可作为待核实假设，不能填入“官方确认”栏。

以下是本轮实际读取的公开页面。动态网页可能更新；查阅日期不是产品发布日，`latest`或`master`也不是冻结版本。官方营销说明表示“官方宣称”，不代表我们实测达到其性能。

| ID | 官方页面 | 支持的观察范围 | 不足以证明 |
| --- | --- | --- | --- |
| U1 | [网易UU远程首页](https://uuyc.163.com/) | 远程桌面、4K/144帧宣传、键鼠/手柄/触屏、多屏、文件、隐私、WOL、无线副屏、协助码和平台入口 | 无头虚拟屏驱动、链路协议、P2P质量迁移、E2E具体设计、任意自建中继、Android被控 |
| R1 | [RustDesk官方仓库说明](https://raw.githubusercontent.com/rustdesk/rustdesk/master/README.md) | 开源客户端、公共/自建ID与Relay、音视频/输入/剪贴板等实现模块、Flutter GUI | 所有后端在所有OS已经实测可用；完整逐连接权限策略 |
| R2 | [RustDesk Self-host](https://rustdesk.com/docs/en/self-host/) | OSS的hbbs/hbbr；打洞失败走Relay；端口；OSS和Pro能力分界 | 质量恶化时同会话迁移；OSS包含Pro多中继/集中设备管理 |
| R3 | [RustDesk Android](https://rustdesk.com/docs/en/client/android/) | Android主控、文件、ID/Relay配置；Android屏幕/文件共享与输入控制权限；系统版本限制 | 绕过系统权限；所有Android版本/厂商都可无人值守被控 |
| S1 | [Sunshine官方文档](https://docs.lizardbyte.dev/projects/sunshine/latest/) | 自托管Moonlight主机、硬件/软件编码、Windows/Linux/macOS捕获方式、配对与Web管理 | 一般远控的文件/剪贴板、Android被控、自建多中继管理与迁移 |
| M1 | [Moonlight官方站点](https://moonlight-stream.org/) | 多平台客户端、完整桌面串流、HDR/4K、PC高帧率、互联网连接方式、GPL客户端 | 所有终端同等性能；完整企业远控生态；任意Relay档案池 |
| M2 | [Moonlight官方Setup Guide](https://raw.githubusercontent.com/wiki/moonlight-stream/moonlight-docs/Setup-Guide.md) | Sunshine配置、配对、桌面入口与互联网连接配置 | NAT条件下总能直连；自动中继故障接管 |
| N1 | [NoMachine Personal Edition](https://www.nomachine.com/everybody/personal-edition) | 当前Personal版本的桌面、媒体、设备、网络与安全功能表，以及订阅/连接数说明 | Enterprise能力属于Personal；产品开源；厂商加密说明等于经我们审计的E2E |
| G1 | [Apache Guacamole 1.6.0用户手册：使用](https://guacamole.apache.org/doc/gug/using-guacamole.html) | 浏览器远控、并发平铺、快捷键/触控、剪贴板、文件、连接共享；依赖底层协议和管理员许可的限制 | 原生host、虚拟屏驱动、P2P/Relay切换、网关无法解密桌面 |

## 易误读处

- RustDesk的OSS与Pro明确分开。R2将Web Console、API、OIDC、LDAP、2FA、设备管理、访问控制和多Relay管理列为Pro；不能自动记到OSS。
- NoMachine当前N1说明Personal按被访问计算机订阅，一条传入桌面连接及同一客户端最多四条终端连接；免费Player/移动客户端与Personal服务端的许可不是同一件事。Enterprise浏览器接入/更高并发不能计为Personal已有。
- N1功能表将文字标签与表示支持的图片列分开。本轮核对了对应图片标志，未把“页面出现该词”本身当作支持。若今后页面结构变化，应重新核对。
- Guacamole是网关体系。G1的文件、磁盘和剪贴板功能受RDP/VNC/SSH、SFTP、浏览器和管理员配置影响；浏览器到网关的HTTPS不证明网关对桌面内容不可见。
- 无线副屏、多显示器选择、OS级无头虚拟显示和独立多用户桌面是不同能力。没有明文依据时不相互替代。
- “打洞失败走中继”和“已有会话质量下降后无感迁移”是不同验收项。R2仅为前者提供直接说明。

## 复核方式

页面可直接打开。辅助工具`inspect-official-page.py`会按公开URL读取文本并显示最终URL与当次原始响应哈希，不保存网页全文、不执行页面脚本；可用于绕开文档导航造成的截断。它不是可信证据的自动裁判，表格中的图示仍需结合正文检查。曾访问的NoMachine `/features`返回404，因此未作为来源。

```text
python docs/research/inspect-official-page.py https://rustdesk.com/docs/en/self-host/ --tail 12000
```

本项目自有代码和单测不能证明竞品行为；竞品官方说明也不能证明本项目实现。功能差距与本项目证据分别见[竞品矩阵](FEATURE-COMPARISON.md)和[实现矩阵](../FEATURE-MATRIX.md)。
