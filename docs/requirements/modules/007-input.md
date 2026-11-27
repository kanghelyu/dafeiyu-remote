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

