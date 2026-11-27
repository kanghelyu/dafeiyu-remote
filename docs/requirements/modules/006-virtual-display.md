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

