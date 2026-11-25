# 参与贡献

感谢你愿意参与「大肥鱼远程」！

## 开发环境

- Rust stable（Windows：`x86_64-pc-windows-gnu` + mingw-w64，或 MSVC + VS Build Tools）
- Flutter 3.x（UI 部分，后续）
- 建议先跑 `cargo test --workspace && cargo run -p dfy-cli -- selftest` 确认环境正常。

## 提交前检查

1. `cargo fmt --all`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `cargo run -p dfy-cli -- selftest`

## 代码规范

- 全部代码须为**独立实现**。禁止复制第三方项目的源码、字段号、私有常量表、图标资源或默认键位。
- 协议变更必须同步修改 `spec/DFYP-v1.md` 并说明兼容性影响（major/minor）。
- 新功能尽量附带测试；涉及协议的消息必须有编解码往返测试。
- 提交信息：`<scope>: <what>`（例：`transport: add DATAGRAM FEC group`）。

## 干净实现（clean-room）声明

参考开源项目时只借鉴**架构与思想**，并用我们自己的命名与协议字段重写实现。提交 PR 时请确认：
- 没有直接搬运代码片段/资源；
- 没有从逆向目标复制私有协议常量或消息名；
- 依赖许可证与 AGPL-3.0-or-later 兼容（禁止 GPLv3/GPL-2.0-only 静态链接）。

## 许可证与 DCO

- 贡献默认采用 **AGPL-3.0-or-later**。
- 提交需带 `Signed-off-by`（`git commit -s`），表示你有权提交该代码。

## 行为准则

尊重、务实、聚焦技术。人身攻击、骚扰、泄露他人隐私信息一律不接受。