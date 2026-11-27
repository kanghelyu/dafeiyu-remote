# 许可证决策（2026-11-27）

## 用户决定

用户明确要求“开源协议不允许商用”。在澄清两种互斥方向后，用户选择：
**改成非商用源码可见（source-available）**。

## 法律/定义约束

- OSI 认定的“开源许可证”**不得**歧视商业使用；因此“开源”与“禁止商用”不能同时成立。
- 原需求 `docs/requirements/000-original-user-prompt.md` 中写的是“完全开源 / Publish … under an appropriate open-source license”，该文件按原始要求**不修改**；本笔记记录后续决策覆盖了该条款。
- 本仓库不再宣称 OSI 开源。

## 执行结果

- `LICENSE`：PolyForm Noncommercial License 1.0.0 全文 + `Required Notice: Copyright (C) 2025-2026 Dafeiyu Remote Project contributors`。
- Cargo 元数据：`license = "LicenseRef-PolyForm-Noncommercial-1.0.0"`（workspace 与显式声明的 crate）。
- 源码/脚本 SPDX：138 处 `SPDX-License-Identifier` 已统一改为 `LicenseRef-PolyForm-Noncommercial-1.0.0`。
- 文档：README、CONTRIBUTING、spec/DFYP-v1.md、design/000、design/020、design/025、driver/README、HANDOFF、中继配置模板与 CLI 启动横幅已同步。
- 协议规范文本 `spec/` 仍按 Apache-2.0 / CC-BY-4.0，与代码许可证独立。

## 后果与边界

- 任何商业使用、商用分发、闭源衍生均不在授权范围。
- 这不等于“产品级安装包可商用发布”；产品级缺口（macOS 原生、Windows IDD 签名、Android/iOS 真机、公网验收、发布签名）仍未完成。
- 按用户此前选择，当前阶段**继续补齐、暂不发布**（commit/push/远端发布均未进行）。
