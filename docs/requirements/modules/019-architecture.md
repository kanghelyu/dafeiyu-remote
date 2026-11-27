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

