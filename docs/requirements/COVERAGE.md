# 原文逐行覆盖校验

此报告验证需求持久化的完整性，不代表任何产品功能已实现或验收。

- 原文：`000-original-user-prompt.md`，UTF-8，共 825 行、23342 字节。
- SHA-256：`3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3`。
- 非空行 583；连续非空段块 242；模块 28。
- 覆盖所有行（包括空行），缺失 0，重复 0，顺序相同。
- 按清单顺序连接全部模块的原始字节，必须与原文逐字节相同。
- 三项补充决策分别保留在模块026、027、028，并校验关键原文。
- `requirements-ledger.jsonl` 为每行保留稳定的 L0001–L0825 编号、类型、原文字段和模块位置；编号表示需求来源，不是实现状态。

复核：`python docs/requirements/split-and-verify.py --check`。

| 模块 | 原文行（含首尾） | 行数 |
| --- | --- | ---: |
| [中文定位、环境授权和三项产品重点](modules/001-positioning.md) | 1–15 | 15 |
| [1. 产品使命与完整生态](modules/002-mission.md) | 16–36 | 21 |
| [2. 合法竞品研究与20项功能矩阵](modules/003-research.md) | 37–91 | 55 |
| [3. 四平台主控、被控与Linux中继](modules/004-platforms.md) | 92–110 | 19 |
| [4. 核心功能标题](modules/005-core-introduction.md) | 111–112 | 2 |
| [4.1 虚拟显示与平台原生实现](modules/006-virtual-display.md) | 113–159 | 47 |
| [4.2 键鼠、国际布局和快捷键映射](modules/007-input.md) | 160–193 | 34 |
| [4.3 剪贴板、权限和逐连接配置](modules/008-clipboard.md) | 194–221 | 28 |
| [4.4 编解码、自适应和连接统计](modules/009-streaming.md) | 222–259 | 38 |
| [5. P2P、自动回退和用户选定中继](modules/010-networking.md) | 260–293 | 34 |
| [6. 独立Linux中继服务](modules/011-relay-server.md) | 294–325 | 32 |
| [6.1 极简安装、维护和诊断](modules/012-relay-deployment.md) | 326–354 | 29 |
| [6.2 不限数量的中继配置](modules/013-relay-profiles.md) | 355–384 | 30 |
| [6.3 质量选择、故障切换与分布式演进](modules/014-relay-selection.md) | 385–410 | 26 |
| [7. Android原生体验和权限限制](modules/015-android.md) | 411–435 | 25 |
| [8. E2E、认证、授权和安全更新](modules/016-security.md) | 436–464 | 29 |
| [9. 主界面、连接界面和中继管理](modules/017-ui.md) | 465–512 | 48 |
| [10. 需评估的20项扩展功能](modules/018-additional-features.md) | 513–539 | 27 |
| [11. 架构候选、17模块和稳定接口](modules/019-architecture.md) | 540–579 | 40 |
| [12. 自主开发与五阶段交付](modules/020-development-phases.md) | 580–642 | 63 |
| [13. 真实平台组合、网络路径和故障测试](modules/021-testing.md) | 643–683 | 41 |
| [14. GitHub、开源合规与发布物](modules/022-release.md) | 684–716 | 33 |
| [15. 15项强制工程原则与真实限制](modules/023-engineering.md) | 717–738 | 22 |
| [16. 最终目标及完成判据](modules/024-final-objective.md) | 739–764 | 26 |
| [三项额外设计决策标题](modules/025-extra-decisions-introduction.md) | 765–768 | 4 |
| [补充1：质量检测、中继池与可恢复会话迁移](modules/026-decision-session-migration.md) | 769–796 | 28 |
| [补充2：独立虚拟显示核心模块](modules/027-decision-virtual-display.md) | 797–810 | 14 |
| [补充3：个人自建优先与多服务器管理](modules/028-decision-relay-modes.md) | 811–825 | 15 |
