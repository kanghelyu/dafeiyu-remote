# 全平台统一图标（2026-11-27）

## 来源与生成

用户给出像素风图标（深蓝圆角底、薄荷绿显示器、橙色鱼、奶油色鼠标指针），要求所有端同步。上传图片未作为可取文件落到工作区，因此按图面**手工重建为 64×64 像素母版**，以 Pillow 最近邻放大生成产物，不复制任何第三方资源。

- 矢量/像素源：`assets/icon-source.svg`
- 生成脚本：`packaging/icons/generate_icons.py`（SHA-256 `0fd3932c2656c2bf5877df18e453532e76510e5f6c523427f5bb1f4b6549e7f3`）
- 预览：`assets/icon-512.png` SHA-256 `c2f8e1783ab7da46078ca2706fc2d7d0215d5aec7cd83183d2bdb51724094b5a`

## 各端产物与接线

| 平台 | 产物 | 接线 |
|---|---|---|
| Windows | `packaging/windows/dfy.ico`（16/32/48/64/128/256）SHA-256 `fb09ae2a937a42b6dcfbcb9d19be7cbce142ccd79df206a176e8a4b280c36971` | `dfy.iss` 新增 `SetupIconFile`；`build-installer.sh` 把 ico 放入暂存目录 |
| macOS | `packaging/macos/DFY.icns`（ic07/ic08/ic09/ic10）SHA-256 `35967df8e86b05bd60362b4dca7d4d842e7159f7bce0b32662d3125a213fe76e` | `build-installer.sh` 把 icns 放入安装包的 docs 目录 |
| Linux | `packaging/linux/dfy-{16,32,48,64,128,256}.png`、`packaging/linux/dafeiyu.desktop` | DEB/RPM 安装 desktop 文件与 hicolor 256×256 图标；`verify-deb.sh` 校验两者存在 |
| Android | `android/app/src/main/res/mipmap-{m,h,xh,xxh,xxxh}dpi/ic_launcher.png` 与 `ic_launcher_round.png` | `AndroidManifest.xml` 从系统 compass 图标改为 `@mipmap/ic_launcher` / `@mipmap/ic_launcher_round` |

## 验证

- ICO 可被 Pillow 打开（256×256）；ICNS 头部为 `icns`，四个 chunk 长度自洽；Android 五个密度尺寸正确；`AndroidManifest.xml` XML 解析通过；`cargo test -p dfy-manager` 7 通过。
- 本机 JDK 已不存在，无法重新构建 APK 验证 Android 资源编译；只做到 XML/资源尺寸静态校验。

## 边界

- 像素母版是**按图面手工重建**，不是用户上传的原始位图逐像素拷贝；视觉接近但不保证完全一致。
- macOS/Windows 安装包图标只在脚本与制品结构层验证，本机没有 ISCC / macOS 打包环境，未做安装后的桌面图标显示验收。
- `dfy-manager` 的 winit 窗口图标尚未接线（当前仍是系统默认窗口图标）。
