# Linux DEB 安装包验收（2026-11-27）

## 范围

- `packaging/linux/build-packages.sh`：新增安装 `prerm`/`postrm` 维护脚本；DEB 仍不启用/启动中继服务，不生成凭据。
- `packaging/linux/debian/prerm`：remove/upgrade/deconfigure 时停止 `dfy-relay.service`。
- `packaging/linux/debian/postrm`：remove/purge 时 disable + daemon-reload；仅在 purge 时尝试清理空的 `/etc/dfy-relay`。
- `packaging/linux/verify-deb.sh`（新增）：无安装地检查控制字段（Package/Version/Architecture/Depends）、数据文件（两个 ELF、systemd unit、配置示例、LICENSE/README）、控制归档中的三个维护脚本，并校验 `.deb.sha256`。

## 真实验收

环境：Debian 13 QEMU guest（TCG），`sudo` 可用；构建产物为 guest 上已成功链接的 ELF（QEMU 下重新链接 `dfy-relay` 曾遇 `ld` Bus error，故复用已构建二进制，属于环境限制而非安装包缺陷）。

完整流程（`.tmp/deb-accept-run5-20261127.log` SHA-256 `567db599a62ab330da52ebbefe98495c0b7b5f6a084586825d4fa1070583f5108`）：

1. `build-packages.sh --binary-dir … --version 0.1.0 --arch x86_64 --format deb` 生成 `dafeiyu_0.1.0_amd64.deb` 与 `.sha256`。
2. `verify-deb.sh` 输出 `DEB verification PASS`。
3. `sudo dpkg -i`：`install ok installed 0.1.0`；`/usr/bin/dfy`、`/usr/bin/dfy-relay` 均可运行并打印中文帮助。
4. `/usr/share/doc/dafeiyu/LICENSE` 存在且含 `PolyForm Noncommercial License 1.0.0`（`LICENSE_OK`）。
5. `sudo dpkg -r dafeiyu`：`deinstall ok config-files`，两个可执行文件均已清除（`BINARIES_CLEAN`）。
6. DEB SHA-256 `384c3cd842d3ede1998a51f633ed496f1d38511add4908d3dd187d6d0c6c695e`；仓库 `LICENSE` SHA-256 `ef8e9bbde48b670a1837c3d6e6a2dea969792991fcb50422359a0918d311ead4`。

## 边界

- 只验证 x86_64 DEB；aarch64 与 RPM 未在本机验证。
- 未验证 systemd 服务实际启停（包刻意不启用服务，需先生成 relay_key）。
- 未验证签名（当前为 unsigned；用户要求非商用 source-available，签名策略未定）。
- Windows/macOS/Android 安装包仍未达到产品级；macOS 签名公证需要 macOS 与 Apple 证书，本环境无法验证。
