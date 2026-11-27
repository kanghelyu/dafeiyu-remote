我建议把这个项目定位为：**开源、跨平台、支持自建中继集群的高性能远程桌面系统**，而不只是一个远程控制软件。你可以自己装wsl和wsa调试跑通，mac端pkg和安卓端apk已经放在下载目录供你解包了，Linux端我没找到，你自己找找好像可以用这个wget -O install.sh https://uuyc.163.com/install.sh && sudo sh install.sh
下面的prompt很长，你必须全部读完全部遵守，一次读不完就分多次，然后模块化保存，做对应部分的时候对应去文件里查看，不要只存在上下文里，太长会丢失信息

你的核心想法有三个真正值得重点投入的地方：

1. **功能体验对标 UU 远程**：尤其是虚拟屏、跨平台快捷键映射、剪贴板同步、多显示器、远程输入和文件传输。
2. **网络架构自主可控**：优先 P2P，连接失败或质量不佳时自动切换到用户指定的中继服务器。
3. **中继服务器极简化**：用户不需要懂 NAT、端口转发、TURN、网络穿透等技术，只需要添加服务器地址、配置密钥，客户端就能使用。

我还补充了几个容易被忽略的关键点：动态码率、弱网恢复、远程唤醒、Android 端特殊权限、Linux 虚拟显示驱动、服务器集群管理、端到端加密，以及开源许可证合规。

下面是一份可以直接交给 Codex、Dot 或其他自主编程 Agent 的完整项目 Prompt。

# Project: 大肥鱼远程 (Big Fat Fish Remote)

## 1. Mission

Build a production-quality, fully open-source, cross-platform remote desktop and remote control system named **大肥鱼远程 (Big Fat Fish Remote)**.

The goal is to achieve functional competitiveness with UU Remote and other leading remote desktop products, while improving their architecture, extensibility, networking flexibility, and self-hosting experience.

This is not a simple prototype or a demonstration project.

The final product must be usable in real-world environments, with reliable connections, excellent responsiveness, strong security, and a polished user experience.

Develop the complete ecosystem:

- Desktop controller applications.
- Desktop host applications.
- Android applications.
- Linux relay server.
- Cross-platform networking and communication infrastructure.
- Installation, configuration, deployment, and update systems.

Publish the finished project on GitHub under an appropriate open-source license.

## 2. Research and Competitive Analysis

Before implementation, conduct a systematic functional analysis of UU Remote and other leading remote desktop applications.

Use legitimate methods such as:

- Public documentation and product demonstrations.
- Observable application behavior.
- Authorized interoperability testing.
- Publicly available technical information.
- Open-source implementations of equivalent functionality.

Study the implementation approaches of:

- RustDesk.
- Sunshine / Moonlight.
- Apache Guacamole.
- NoMachine.
- Other relevant open-source remote desktop projects.

Do not blindly copy existing implementations.

Use the best available open-source components as foundations where appropriate, while designing a better integrated architecture.

Respect intellectual property, software licenses, and applicable laws. Do not extract proprietary source code, bypass authentication, or redistribute proprietary components.

### Produce a feature comparison matrix

Identify every observable feature, including:

1. Remote desktop streaming.
2. Virtual display creation and management.
3. Multi-monitor support.
4. Resolution and refresh-rate management.
5. Keyboard and mouse input.
6. Cross-platform keyboard shortcut translation.
7. Clipboard synchronization.
8. File transfer.
9. Audio transmission.
10. Multiple simultaneous sessions.
11. Remote reboot and reconnection.
12. Unattended access.
13. Remote device management.
14. Connection quality adaptation.
15. NAT traversal.
16. P2P connections.
17. Relay connections.
18. Connection security.
19. Android remote control.
20. Session management and permissions.

Identify missing features and opportunities to improve them.

Maintain a feature implementation matrix throughout development. Never claim a feature is complete without testing it.

## 3. Supported Platforms

Support the following platforms.

| Platform      | Controller | Host |
| ------------- | ---------- | ---- |
| Windows 10/11 | Yes        | Yes  |
| Linux         | Yes        | Yes  |
| macOS         | Yes        | Yes  |
| Android       | Yes        | Yes  |

Linux is also the only required platform for the initial relay server.

Design the architecture so that additional platforms can be supported later.

The desktop applications should share as much code as possible.

Android must have a proper native user experience and account for Android-specific permissions and restrictions.

## 4. Core Remote Desktop Features

### 4.1 Virtual Display Technology — Highest Priority

Virtual display support is one of the most important features.

Implement a robust virtual monitor system.

Requirements:

- Create virtual displays without requiring a physical monitor.
- Support multiple virtual displays.
- Dynamically change resolution.
- Dynamically change refresh rate where supported.
- Support common resolutions, including 1080p, 1440p, and 4K.
- Support portrait and landscape orientations.
- Manage virtual display creation and destruction.
- Support headless operation.
- Allow remote control when the physical monitor is disconnected.
- Support independent display sessions.
- Provide reliable recovery after display driver failures.
- Avoid unnecessary modification of physical monitor settings.

Platform-specific implementation:

**Windows**

- Investigate Windows Indirect Display Driver (IDD).
- Support virtual monitors and headless operation.
- Handle display enumeration and resolution changes.

**Linux**

- Investigate DRM/KMS, VKMS, Wayland virtual outputs, and X11 virtual displays.
- Support headless virtual desktop environments.
- Avoid requiring a physical display.

**macOS**

- Investigate supported virtual display APIs and appropriate system extensions.
- Respect macOS security and permission requirements.

**Android**

- Support remote screen capture through MediaProjection.
- Clearly handle user authorization and background restrictions.

Provide a unified virtual display management interface across platforms.

### 4.2 Keyboard and Mouse

Implement a complete cross-platform input translation layer.

Requirements:

- Keyboard input.
- Mouse movement.
- Mouse buttons.
- Scroll wheel.
- Keyboard combinations.
- Function keys.
- Multimedia keys where supported.
- International keyboard layouts.
- IME and non-English input.
- Different keyboard layouts between controller and host.
- Modifier key translation.

Special attention must be paid to:

- Ctrl / Command differences.
- Alt / Option differences.
- Windows key / Super key differences.
- Ctrl+C, Ctrl+V, Ctrl+Alt+Del and other system shortcuts.
- macOS Command-based shortcuts.
- Linux desktop environment shortcuts.
- Special keys and key combinations.

Design a configurable keyboard mapping system.

Users should be able to create, modify, export, and import shortcut mappings.

Prevent stuck keys and ensure modifier states are correctly released after disconnection.

### 4.3 Clipboard Synchronization

Implement bidirectional clipboard synchronization.

Support:

- Plain text.
- Unicode.
- Rich text where practical.
- Images.
- File references where supported.
- Large clipboard contents.
- Clipboard synchronization between different operating systems.

Requirements:

- Configurable synchronization direction.
- Automatic synchronization.
- Manual synchronization.
- Conflict handling.
- Size limits.
- Security restrictions.
- Clear user notifications.

Clipboard synchronization must be independently configurable for every connection.

Never silently transfer sensitive clipboard contents without the user's configured permission.

### 4.4 Streaming and Performance

Implement a high-performance remote desktop streaming pipeline.

Investigate:

- Hardware encoding and decoding.
- H.264.
- H.265 / HEVC.
- AV1 where hardware and software support permit.
- Adaptive bitrate.
- Dynamic resolution.
- Frame-rate adaptation.
- Network congestion control.
- Low-latency transport.
- Region-based updates.
- Efficient screen capture.

Prioritize responsiveness and visual quality.

Provide configurable performance profiles:

- Low latency.
- Balanced.
- High quality.
- Low bandwidth.

Display connection statistics:

- Round-trip latency.
- Packet loss.
- Bitrate.
- Frame rate.
- Resolution.
- Encoding and decoding latency.
- Transport mode.
- Current relay server.

## 5. Networking Architecture

This is one of the most important architectural requirements.

The system must support three connection modes.

### Mode A: Direct P2P

Attempt direct peer-to-peer connectivity first.

Investigate:

- STUN.
- ICE.
- UDP hole punching.
- QUIC.
- Other suitable NAT traversal mechanisms.

### Mode B: Automatic Relay Fallback

If direct P2P cannot be established, or the connection quality is unacceptable, automatically switch to a relay server.

The transition must be transparent to users.

Avoid unnecessary session interruption.

### Mode C: User-Selected Relay

Users must be able to configure their own relay servers.

This feature is a major product differentiator.

The relay server must not be hardcoded into the client.

## 6. Relay Server System

Develop a standalone Linux relay server.

The server must be easy to deploy, configure, upgrade, and maintain.

### Server requirements

- Linux x86_64 initially.
- ARM64 support where practical.
- Single-binary deployment if feasible.
- Docker deployment.
- Simple configuration.
- Automatic startup.
- Systemd integration.
- TLS support.
- Secure authentication.
- Connection management.
- Concurrent sessions.
- Bandwidth monitoring.
- Resource monitoring.
- Structured logging.
- Health checks.
- Graceful shutdown.
- Automatic restart.
- Rate limiting.
- Abuse prevention.

The relay server should primarily forward encrypted traffic.

It must not require access to users' desktop contents or plaintext session data.

### 6.1 Extremely Simple Deployment

The installation experience is critical.

A typical deployment should require only:

1. Obtain a Linux server.
2. Run one installation command.
3. Configure the server address and authentication secret.
4. Start the service.
5. Add the server to the client.

Provide:

- One-command installation.
- Interactive setup wizard.
- Automatic configuration generation.
- Automatic TLS certificate management where applicable.
- Firewall and port diagnostics.
- Installation verification.
- One-command upgrade.
- One-command uninstall.

Provide Docker Compose as an alternative deployment method.

Document the exact ports, protocols, firewall rules, and resource requirements.

Never expose management interfaces or administrative credentials unnecessarily.

### 6.2 Multiple Relay Servers

A user must be able to add an unlimited number of relay server profiles in the client.

Each profile should contain:

- Display name.
- Server address.
- Port.
- Authentication information.
- TLS configuration.
- Optional region.
- Connection priority.
- Health status.

Users must be able to:

- Add servers.
- Delete servers.
- Edit servers.
- Test connectivity.
- Switch servers manually.
- Select automatic server selection.
- Set a preferred server.
- Import and export server configurations.

The client must clearly show which server is currently being used.

Do not force users to use a centralized official server.

### 6.3 Relay Selection and Failover

Implement intelligent relay selection.

Consider:

- Latency.
- Packet loss.
- Bandwidth.
- Geographic location.
- Server availability.
- User preference.

Provide configurable selection strategies:

- Manual.
- Lowest latency.
- Preferred server first.
- Automatic failover.

If the selected relay fails, attempt another configured relay.

Maintain secure session establishment and avoid exposing credentials during server switching.

Design the system so that relay servers can eventually form a distributed infrastructure.

## 7. Android Requirements

Android is a first-class platform, not an afterthought.

Implement:

- Remote desktop viewing.
- Touch input.
- Mouse emulation.
- External keyboard support.
- Clipboard synchronization.
- File transfer.
- Virtual keyboard.
- Landscape and portrait support.
- Adaptive resolution.
- Connection management.
- Relay server selection.
- Session security.

Investigate Android's limitations on background execution, screen capture, accessibility, and input injection.

Clearly distinguish features that require explicit user authorization or accessibility permissions.

Do not promise unrestricted control when the operating system does not permit it.

## 8. Security Architecture

Security must be designed from the beginning.

Requirements:

- End-to-end encryption.
- Mutual authentication.
- Device identity.
- Secure key exchange.
- Secure credential storage.
- Session authorization.
- Optional two-factor authentication.
- Device pairing.
- Access control.
- Session expiration.
- Replay protection.
- Protection against unauthorized relay impersonation.
- Secure update mechanism.
- Audit logs.

The relay server must not be able to decrypt remote desktop sessions.

Do not store passwords or authentication secrets in plaintext.

Provide clear controls for revoking trusted devices and terminating active sessions.

Implement security testing before public release.

## 9. User Interface

The product should look and feel like a polished commercial remote desktop application.

Avoid a developer-oriented interface.

### Main interface

Provide:

- Device list.
- Online/offline status.
- Device identification.
- Recent connections.
- Favorites.
- Search.
- Connection history.
- Relay configuration.
- Settings.
- Account/device management where applicable.

### Connection interface

Provide:

- Full-screen mode.
- Multi-monitor switching.
- Resolution selection.
- Display management.
- Keyboard mapping.
- Clipboard controls.
- File transfer.
- Audio controls.
- Image quality settings.
- Connection statistics.
- Reconnect.
- Session permissions.

### Relay management interface

Make relay configuration exceptionally simple.

The user should not need to understand networking protocols.

A normal user should be able to add a server and use it within a minute.

Provide useful diagnostics when a server cannot be reached.

## 10. Additional Features Worth Implementing

Evaluate and implement the following where appropriate:

- Remote file manager.
- Drag-and-drop file transfer.
- Multiple simultaneous connections.
- Remote audio.
- Two-way audio where practical.
- Remote terminal with explicit authorization.
- Remote reboot and shutdown.
- Wake-on-LAN where supported.
- Device groups.
- Trusted devices.
- Session recording with user consent.
- Privacy mode.
- Blank physical display where supported.
- Connection permissions.
- Multi-user access.
- Remote system information.
- Portable client.
- Automatic updates.
- Self-hosted device directory.
- Centralized device management for future enterprise use.

These features must not compromise the core architecture or delay the essential functionality unnecessarily.

## 11. Suggested Architecture

Do not blindly follow this proposal. Evaluate alternatives before making architectural decisions.

A possible architecture:

- Rust: Core networking, relay server, protocol, and cross-platform shared components.
- Native platform integrations: Windows, Linux, macOS.
- Kotlin: Android.
- Tauri or another suitable framework: Desktop interface.
- WebRTC / QUIC: Evaluate for media and transport.
- Hardware encoding APIs: Platform-specific implementation.
- Protocol Buffers or another appropriate serialization format.

Separate the system into:

1. Core.
2. Capture.
3. Virtual Display.
4. Encoder.
5. Decoder.
6. Input.
7. Clipboard.
8. File Transfer.
9. Audio.
10. Networking.
11. P2P.
12. Relay.
13. Device Discovery.
14. Authentication.
15. Desktop UI.
16. Android UI.
17. Server Management.

Define stable interfaces between modules.

Avoid unnecessary microservices.

Keep the relay server lightweight and independent of the desktop applications.

## 12. Development Methodology

Work autonomously and systematically.

Do not stop after producing an architecture document or a prototype.

### Phase 1: Research and Architecture

- Complete competitive analysis.
- Identify suitable open-source foundations.
- Compare licenses.
- Evaluate platform APIs.
- Define the communication protocol.
- Define the security model.
- Produce architecture diagrams.
- Create the feature matrix.
- Establish measurable performance targets.

### Phase 2: Core MVP

Implement:

- Windows host and controller.
- Linux host and controller.
- Basic desktop streaming.
- Keyboard and mouse.
- Clipboard synchronization.
- Direct P2P.
- Linux relay server.
- Manual relay configuration.
- Automatic relay fallback.

### Phase 3: Advanced Desktop Features

Implement:

- Virtual displays.
- Multiple monitors.
- Advanced keyboard mappings.
- File transfer.
- Audio.
- Adaptive streaming.
- Automatic relay selection.
- Multi-server management.

### Phase 4: macOS and Android

Complete native platform integrations.

Test real cross-platform connections.

### Phase 5: Production Quality

- Security audit.
- Performance optimization.
- Stress testing.
- Network failure testing.
- Installation testing.
- Upgrade testing.
- UI refinement.
- Documentation.
- Release automation.

## 13. Testing Requirements

Create automated tests and a reproducible testing environment.

Test all combinations of:

- Windows to Windows.
- Windows to Linux.
- Windows to macOS.
- Linux to Windows.
- Linux to Linux.
- macOS to Windows.
- Android to Windows.
- Android to Linux.
- Android to macOS.

Test both P2P and relay modes.

Test:

- Clipboard in both directions.
- Keyboard shortcut translation.
- Different keyboard layouts.
- Virtual display creation.
- Resolution switching.
- Multi-monitor operation.
- File transfer.
- Audio.
- Network interruptions.
- Relay failure.
- Server switching.
- Reconnection.
- High latency.
- Packet loss.
- Bandwidth restrictions.
- Long-running sessions.

Maintain a test coverage report.

Never mark an untested feature as complete.

## 14. GitHub and Open-Source Release

Publish the project as:

**大肥鱼远程 | Big Fat Fish Remote**

Create a clean GitHub repository.

Include:

- README.md.
- English and Chinese documentation.
- Architecture documentation.
- Installation instructions.
- Server deployment guide.
- Client configuration guide.
- API and protocol documentation.
- Security policy.
- Contribution guide.
- License.
- Changelog.
- Issue templates.
- CI/CD.
- Release workflow.

Choose a license after reviewing all dependencies and their compatibility.

Provide reproducible builds where practical.

Publish binaries for supported platforms.

Do not include secrets, private keys, proprietary binaries, or copyrighted assets.

## 15. Engineering Principles

These requirements are mandatory.

1. Do not merely produce plans. Implement working software.
2. Do not stop at an MVP if substantial work remains.
3. Do not claim completion without evidence.
4. Do not implement fake or placeholder functionality and label it complete.
5. Prioritize architectural correctness over superficial feature counts.
6. Reuse mature open-source components when appropriate.
7. Keep dependencies maintainable.
8. Avoid unnecessary complexity.
9. Document important engineering decisions.
10. Test continuously.
11. Keep the repository buildable.
12. Commit working milestones regularly.
13. Maintain a concise development tracker.
14. Identify blockers and resolve them autonomously where possible.
15. Do not ask for approval for routine engineering decisions.

When a feature is blocked by hardware, operating-system restrictions, or missing permissions, document the exact limitation and provide the best supported alternative.

## 16. Final Objective

The final objective is to deliver a genuinely useful remote desktop ecosystem.

The defining characteristics should be:

- Excellent virtual display technology.
- Reliable cross-platform keyboard mapping.
- Seamless bidirectional clipboard synchronization.
- Responsive remote desktop streaming.
- Reliable P2P with transparent relay fallback.
- Arbitrary user-configured relay servers.
- Extremely simple relay deployment.
- Native support for Windows, Linux, macOS, and Android.
- Strong security.
- A polished user experience.
- Complete open-source availability.

The relay system should be particularly easy to deploy and use. Users should be able to operate their own infrastructure without becoming networking experts.

**Think beyond feature parity. Build something technically better, more flexible, easier to deploy, and more open than the products being studied.**

Start with research and a concrete implementation plan, then proceed directly to engineering. Maintain a short, continuously updated tracker showing completed work, verified tests, remaining work, and blockers.

Do not declare the project finished until the supported features have been implemented, tested, documented, and released.

## **我额外建议：三个设计决策**

这三个决定会显著影响项目最终质量。

### **1. 不要把中继服务器设计成简单的流量转发程序**

建议采用：

优先

**P2P 直连**

低延迟，尽可能减少服务器流量

**连接质量检测**

延迟、丢包、带宽、连接稳定性

**P2P 保持**

质量合格

**Relay 接管**

连接失败或质量不合格

**用户配置的中继服务器池**

多服务器、健康检测、自动切换、优先级管理

特别注意：P2P 和 Relay 之间的切换应该设计成可恢复的会话迁移，而不只是断开后重新连接。

### **2. 虚拟屏必须成为独立的核心模块**

我认为这是整个项目最值得投入技术资源的地方之一。

尤其要做好：

- 无显示器远程开机后建立虚拟桌面。
- 多个虚拟屏幕独立控制。
- 远程调整分辨率，不影响物理显示器。
- 不同操作系统的虚拟显示实现统一抽象。
- 虚拟屏驱动异常后自动恢复。

这里要注意，Windows、Linux 和 macOS 的实现机制差异很大，不能指望一个通用 API 解决所有问题。

### **3. 中继服务器最好支持两种使用模式**

**个人自建模式**

优先开发

用户自己购买一台 Linux VPS，运行安装脚本，客户端输入服务器地址和密钥即可使用。

**多服务器管理模式**\
用户可以添加任意数量的服务器，例如国内、香港、日本、美国的 VPS。客户端自动测试延迟，选择最合适的服务器，也可以手动指定。

这样未来甚至可以发展成一个独立的中继基础设施生态。


建议先把 Windows/Linux 的完整远控链路和 Linux 中继跑通，再扩展 macOS 和 Android。尤其是虚拟屏、输入注入、系统权限，这些都需要真正的原生开发。
