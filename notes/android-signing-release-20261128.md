# 大肥鱼远程 Android 签名与 CI 修复发布（2026-11-28）

## 范围

- 发布版本：基础版本 `v0.2.1`；公开 tag 为 `v0.2.1+ci1`（Linux CI 修复构建，不是 SemVer prerelease）；Android `versionName=0.2.1`、`versionCode=3`，包名 `remote.dafeiyu`。
- `minSdk=29`、`targetSdk=36`，原生 ABI 为 `arm64-v8a` 与 `x86_64`。
- Windows/macOS 正式代码签名、Apple 公证、Google Play 发布和 Android 真机验收不在本轮范围内。

## 签名材料

- 使用 Android Studio 自带 JDK 的 `keytool` 生成专用 PKCS12 keystore；别名 `dafeiyu-release`。
- 证书主题：`CN=Dafeiyu Remote, O=Dafeiyu Remote Project`；RSA 3072；`SHA384withRSA`。
- 证书有效期：2026-11-28 至 2054-04-15。
- 证书 SHA-256：`1ee8ac5891bca0e00d4f4029059c25a8060e796bc307d6ac07d307c73aaff3b8`。
- keystore 与凭据位于仓库外：`C:\Users\21903\.android\dafeiyu-release.jks`、`C:\Users\21903\.android\dafeiyu-release-credentials.properties`；证书观察文件为同目录 `dafeiyu-release-certificate.txt`。
- Windows ACL 检查显示 SYSTEM、Administrators 与当前用户具有完全控制权限；未把这些文件或任何密码写入仓库。
- `.gitignore` 排除 `*.jks`、`*.keystore`、`key.properties` 与 `dafeiyu-release-credentials.properties`。
- GitHub Actions secrets：`ANDROID_KEYSTORE_BASE64`、`ANDROID_KEYSTORE_PASSWORD`、`ANDROID_KEY_ALIAS`、`ANDROID_KEY_PASSWORD`；只核对名称，不记录值。

## 构建与验证

- JDK 17、Android SDK/NDK 下执行 `packaging/android/build.sh --version 0.2.1 --signing required --output-dir .tmp/android-release-v0.2.1`。
- Gradle：`BUILD SUCCESSFUL in 32s`，100 个任务中 98 执行、2 个为最新状态。
- APK：`apksigner verify --verbose --print-certs` 通过；v1 为 false，v2 为 true，v3/v3.1/v4 为 false；单一签名者与上述证书指纹一致。
- AAB：`jarsigner -verify` 退出 0。自签名证书没有公共 CA 链，因此工具会输出 PKIX 警告；这不是签名完整性失败。
- ZIP 完整性、`AndroidManifest.xml`、`resources.arsc`、`classes.dex` 和两个 ABI 的 `libdfy_android.so` 均由打包脚本检查。
- AAPT 独立读回：`remote.dafeiyu`、versionCode 3、versionName 0.2.1、min/target SDK 29/36、arm64-v8a/x86_64。
- `./gradlew :app:testDebugUnitTest :app:lintDebug --offline -x cargoBuild`：35 测试、0 失败、0 错误、0 跳过；lint 0 错误、19 警告。
- `cargo fmt --all -- --check`、`cargo check/test --locked --offline -p dfy-manager -p dfy-ui` 与 strict Clippy `--all-targets -- -D warnings` 通过。
- 首次 Release CI 的 Linux Manager 因 `rfd` 关闭默认异步运行时、导致 `zbus` 未启用 Tokio/async-io 而失败；已显式启用 `rfd/tokio`，依赖树确认 `zbus/tokio` 生效，Windows Manager/Viewer 检查与测试再次通过。本机 Linux 交叉检查仅因缺少 `x86_64-linux-gnu-gcc` 中止，不能替代 Ubuntu CI 实跑。
- 修复后的 Release workflow run [`46121415301`](https://github.com/kanghelyu/dafeiyu-remote/actions/runs/46121415301) 全部 14 个 job 成功，包括 x86_64/aarch64 Linux Manager 构建、Android 强制签名与 GitHub Release 发布。
- `MSYS=winsymlinks:nativestrict python packaging/tests/smoke.py`、需求台账 `--check`、SBOM `--check` 与 `git diff --check` 全部通过。

## 本地候选产物

以下哈希属于 Windows 本地候选构建，不代表 CI 重建产物逐字节相同。

| 文件 | 字节 | SHA-256 |
| --- | ---: | --- |
| `dafeiyu-0.2.1-release.apk` | 13773410 | `56f605f21c1ac26419a04c450c82337cb9850a04a86c64d3cfd9f7105b1b00f6` |
| `dafeiyu-0.2.1-release.aab` | 10027695 | `13b8220eaa461624de5606cb2881b5092fde7c650e0314f71af9c861e164ddd2` |
| `dafeiyu-0.2.1-debug.apk` | 14710724 | `ca1276ea0217964fd03475a2b398d227dc008d65ecab6b8bde284579bf637bc7` |
| `BUILD-INFO.txt` | 201 | `66e6d26da058d024d3b739cc1069fc69d4375736c4202f8a4fc79f01f94ef3e0` |

## 公开发布产物

公开 Release：[大肥鱼远程 v0.2.1+ci1](https://github.com/kanghelyu/dafeiyu-remote/releases/tag/v0.2.1%2Bci1)，发布于 `2026-11-28T07:40:48Z`，不是 draft 或 prerelease。发布资产中没有任何带 `unsigned` 标记的 Android APK/AAB。

以下字节数与 SHA-256 来自重新下载的公开资产，并与发布页 `SHA256SUMS.txt` 逐项一致：

| 文件 | 字节 | SHA-256 |
| --- | ---: | --- |
| `dafeiyu-v0.2.1+ci1-release.apk` | 13770954 | `f14e54d38ff3bee540970d5e5f493544ac9b518a20f8696f15714a395e4da49e` |
| `dafeiyu-v0.2.1+ci1-release.aab` | 10027332 | `90e5b2fccee990949c6e1d79dc34859727f72f659f9ba7397dd329ad007bcfcc` |
| `dafeiyu-v0.2.1+ci1-debug.apk` | 14708268 | `4e33e9ed66e9606fc92898d8fd7fe90556650c42dfa1e17bb40f3fa54aab3def` |

公开产物独立复核：

- `apksigner verify --verbose --print-certs`：Release APK 的 v2 签名有效，单一 RSA 3072 签名者，证书 SHA-256 为 `1ee8ac5891bca0e00d4f4029059c25a8060e796bc307d6ac07d307c73aaff3b8`。
- `jarsigner -verify`：Release AAB 退出 0；自签名证书没有公共 CA 链，PKIX 路径警告不代表签名完整性失败。
- AAPT：`remote.dafeiyu`、versionCode 3、versionName 0.2.1、min/target SDK 29/36、arm64-v8a/x86_64，应用标签为“大肥鱼远程”。
- Python `zipfile.testzip()` 与必需载荷检查：Release APK、Release AAB、Debug APK 均通过，两个 ABI 的 `libdfy_android.so` 均存在。

## 发布接线与边界

- Release workflow 对 Android 使用 `--signing required`；四项凭据缺失即失败，不再发布带 `unsigned` 标记的 release APK/AAB。
- Windows 上改用 `java -jar <build-tools>/lib/apksigner.jar`，避免 `apksigner.bat` 在含空格的 `JAVA_HOME` 路径下拆词。
- 公开 Release 资产名由 tag 生成：`dafeiyu-v0.2.1+ci1-release.apk`、`dafeiyu-v0.2.1+ci1-release.aab`、`dafeiyu-v0.2.1+ci1-debug.apk`；已重新下载并核对签名、版本与 `SHA256SUMS.txt`。
- 签名只证明包完整性与发布者身份，不证明真机安装升级、录屏/输入/跨机功能、弱网长稳或 Google Play 接受。

## 双启动入口缺陷与单入口修复候选（2026-11-28）

- 根因：公开 `v0.2.1+ci1` APK 的二进制清单同时包含 `remote.dafeiyu.MainActivity` 与 `remote.dafeiyu.ViewerLauncher` 两个 `MAIN`/`LAUNCHER` 入口，因此安装后出现“大肥鱼远程”和“大肥鱼远程查看端”两个图标。API36 x86_64 模拟器安装公开包后查询到 2 个 `remote.dafeiyu` 启动项，复现成功。
- 修复：删除 `ViewerLauncher` 别名，只在 `MainActivity` 保留唯一启动入口；主界面新增“进入查看端”卡片与按钮，内部显式启动 `ViewerActivity`，查看端功能不再依赖第二个桌面图标。
- 安装级验证同时抓到并修复了一个独立启动阻断：`MainActivity` 的字段初始化在 Context 绑定前调用 `getString()`，点击图标会抛出 `NullPointerException`。改为在 `onCreate` 读取状态后渲染，不提前访问资源。
- 候选版本：`versionName=0.2.1`、`versionCode=4`，包名仍为 `remote.dafeiyu`。同包名、同签名覆盖安装公开 ci1 成功；升级后查询仅 1 个启动项。卸载后全新安装也仅 1 个启动项，主界面冷启动成功，点击“进入查看端”后 `ViewerActivity` 成为前台 Activity，crash buffer 为空。
- 静态与自动检查：AAPT 二进制清单仅 1 个 `LAUNCHER`；36 个单测全部通过、lint 0 错误/19 警告；`packaging/tests/smoke.py` 与 `git diff --check` 通过；Release APK v2 签名有效，证书 SHA-256 仍为 `1ee8ac5891bca0e00d4f4029059c25a8060e796bc307d6ac07d307c73aaff3b8`；AAB `jarsigner -verify` 退出 0。

本地候选产物位于 `.tmp/android-single-launcher-v0.2.1-ci2/`，不代表已经公开：

| 文件 | 字节 | SHA-256 |
| --- | ---: | --- |
| `dafeiyu-0.2.1+ci2-release.apk` | 13774614 | `681f4572226a93716e62fbc2e9830195e0c35bb088be157c7991d02e5668bc00` |
| `dafeiyu-0.2.1+ci2-release.aab` | 10028193 | `d81bd8c401d352ca0b15431fc7d3794d9673a7da793af490a235bb7eb6806eac` |
| `dafeiyu-0.2.1+ci2-debug.apk` | 14711932 | `f029c67247a95479d352755e656768ff3be8fe94f0271a2e9443d28974525e1d` |

本轮只生成本地候选，未提交、推送、移动旧 tag 或更新公开 Release。API36 模拟器结果不能替代物理真机、所有 OEM 启动器、Google Play、录屏/输入/跨机或升级数据保留验收；公开 ci1 在修复候选发布前仍保持原状。

## Release 只保留安装包与介绍页重写（2026-11-28）

- 公开 `v0.2.1+ci1` 的 Release 同时挂有安装包、ZIP、AAB、Debug APK、`SHA256SUMS.txt`、SBOM 与 notices。源码、构建脚本和文档已经随 tag 进入仓库，发布页不再重复挂这些开发/辅助产物。
- `release.yml` 的发布任务改为从 CI artifacts 中只整理 10 个安装包：Windows Setup、macOS Intel/Apple Silicon 各 DMG+PKG、Linux amd64/arm64 各 DEB+RPM、Android 已签名 Release APK。AAB、Debug APK、ZIP、校验和、SBOM 和 notices 仍可作为 CI artifact 获取，但不上传到公开 Release。
- Android 构建参数由 `--version "$RELEASE_TAG"` 改为 `--version "${RELEASE_TAG#v}"`，统一公开文件名为 `dafeiyu-0.2.1+ci2-release.apk`，不再产生 `dafeiyu-v0.2.1+ci2-release.apk` 这种带双版本前缀的名称。
- 发布说明改为中文直链列表，每个平台/架构一个安装包链接；同时保留口令、签名/公证、Wayland 和非商用许可证边界。
- 本地用代表性产物模拟发布整理：`release-assets/` 恰好 10 个文件，生成的 Markdown 恰好 10 个 `releases/download/...` 直链；`release.yml` YAML 解析通过，两个仓库 `git diff --check` 通过。模拟不等于真实 GitHub Actions 构建或公开下载验收。
- 个人主页 `apps/dafeiyu/index.html` 重写为“从另一台设备，操作你的电脑”的直白介绍，说明能做什么、三步怎么用、实际界面、各端安装包和源码/许可证；`projects.html` 与 `assets/js/projects.js` 的中英文项目卡片同步为同一口径。
- 介绍页只使用检查后的当前应用截图：Windows Manager 为重新构建 `target/release/dfy-manager.exe` 后实际起窗捕获的 1122×660 画面；Android 为 ci2 候选在 API36 x86_64 模拟器 `emulator-5556` 捕获的 1080×2400 画面，页面明确标注“模拟器画面”。未使用含学生姓名、成绩或会话口令的旧截图。
- 使用本机 Microsoft Edge headless + CDP 检查桌面与 390px 移动端；390px 下 `innerWidth=390`、`clientWidth=390`、`scrollWidth=390`、`bodyScrollWidth=390`，下载区经滚动定位后截图复核。该检查是浏览器渲染证据，不替代 Android 真机验收。
- 此节记录的是本地完成、待提交/发布状态；真实 CI run、公开 Release URL、10 个公开直链及公开产物哈希须在发布后另行实测补记。
