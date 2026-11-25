// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 快捷键四态决策（控制端）。
//!
//! 控制端按下组合键时，需要决定这一击是「只发远端」「只在本机生效」「两边都生效」
//! 还是「完全拦截」。决策结果驱动输入采集线程与本地快捷键分发：
//!
//! | 结果 | 含义 |
//! |---|---|
//! | [`ShortcutDecision::SendToRemote`] | 只发远端（默认，远端应用收到按键） |
//! | [`ShortcutDecision::LocalOnly`] | 只在本机处理，不转发（回环标记事件防死循环） |
//! | [`ShortcutDecision::LocalAndRemote`] | 本机执行功能动作，同时也转发远端 |
//! | [`ShortcutDecision::Blocked`] | 系统级按键由本机独占，完全不转发远端 |

/// 快捷键决策结果（四态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutDecision {
    /// 只发远端。
    SendToRemote,
    /// 只在本机处理。
    LocalOnly,
    /// 本机与远端都处理。
    LocalAndRemote,
    /// 本机系统独占，不转发。
    Blocked,
}

/// 决策上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShortcutContext {
    /// 当前修饰键位图（HID 语义，bit0=LCtrl … bit7=RGUI）；预留给组合键规则。
    pub modifiers: u8,
    /// 该事件是否由我们自己的注入产生（用于防回环）。
    pub injected_by_us: bool,
    /// 当前会话是否为「仅控制端」模式（例如本地预览窗口拥有焦点时的本地快捷操作）。
    pub controller_only: bool,
}

/// 快捷键配置表。列表元素均为 HID Usage ID。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortcutTable {
    /// 仅在 `controller_only` 模式下才转发到远端的键。
    pub controller_only: Vec<u16>,
    /// 本机有功能动作、同时也要转发远端的键。
    pub local_function: Vec<u16>,
    /// 系统级按键（由本机操作系统独占处理），不转发远端。
    pub system_local: Vec<u16>,
}

impl ShortcutTable {
    /// 创建空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 判断该 usage 是否在 `controller_only` 列表。
    #[must_use]
    pub fn is_controller_only(&self, usage: u16) -> bool {
        self.controller_only.contains(&usage)
    }
}

/// 快捷键四态决策。
///
/// 判定顺序（先命中先返回）：
/// 1. `ctx.injected_by_us` → [`ShortcutDecision::LocalOnly`]（我们注入的合成事件只在本机消化，
///    绝不再次转发，防止远程回环）；
/// 2. `controller_only` 列表命中且 `ctx.controller_only` → [`ShortcutDecision::SendToRemote`]
///    （控制端专属快捷键直达远端）；
/// 3. `system_local` 列表命中 → [`ShortcutDecision::Blocked`]（系统键本地独占，不转发）；
/// 4. `local_function` 列表命中 → [`ShortcutDecision::LocalAndRemote`]（本机动作 + 转发）；
/// 5. 其余 → [`ShortcutDecision::SendToRemote`]。
#[must_use]
pub fn decide_shortcut(
    table: &ShortcutTable,
    usage: u16,
    ctx: &ShortcutContext,
) -> ShortcutDecision {
    if ctx.injected_by_us {
        return ShortcutDecision::LocalOnly;
    }
    if ctx.controller_only && table.controller_only.contains(&usage) {
        return ShortcutDecision::SendToRemote;
    }
    if table.system_local.contains(&usage) {
        return ShortcutDecision::Blocked;
    }
    if table.local_function.contains(&usage) {
        return ShortcutDecision::LocalAndRemote;
    }
    ShortcutDecision::SendToRemote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ShortcutTable {
        ShortcutTable {
            controller_only: vec![0x14],          // Q
            local_function: vec![0xE3, 0x2B],     // LGUI、Tab
            system_local: vec![0x29, 0x46, 0x2B], // Esc、PrintScreen、Tab
        }
    }

    #[test]
    fn injected_events_stay_local() {
        let ctx = ShortcutContext {
            modifiers: 0x08,
            injected_by_us: true,
            controller_only: true,
        };
        // 即使命中所有列表也必须 LocalOnly。
        assert_eq!(
            decide_shortcut(&table(), 0x29, &ctx),
            ShortcutDecision::LocalOnly
        );
        assert_eq!(
            decide_shortcut(&table(), 0x14, &ctx),
            ShortcutDecision::LocalOnly
        );
    }

    #[test]
    fn controller_only_list_forwards_in_controller_mode() {
        let ctx = ShortcutContext {
            controller_only: true,
            ..ShortcutContext::default()
        };
        assert_eq!(
            decide_shortcut(&table(), 0x14, &ctx),
            ShortcutDecision::SendToRemote
        );
    }

    #[test]
    fn controller_only_list_falls_through_without_controller_mode() {
        // 非 controller_only 模式时，Q 不再特殊；但它也不在其它列表，默认转发。
        let ctx = ShortcutContext::default();
        assert_eq!(
            decide_shortcut(&table(), 0x14, &ctx),
            ShortcutDecision::SendToRemote
        );
        // 若同一键还在 system_local 中，则应落到 Blocked 分支。
        let mut t = table();
        t.system_local.push(0x14);
        assert_eq!(decide_shortcut(&t, 0x14, &ctx), ShortcutDecision::Blocked);
    }

    #[test]
    fn system_keys_are_blocked() {
        let ctx = ShortcutContext::default();
        assert_eq!(
            decide_shortcut(&table(), 0x29, &ctx),
            ShortcutDecision::Blocked
        );
        assert_eq!(
            decide_shortcut(&table(), 0x46, &ctx),
            ShortcutDecision::Blocked
        );
    }

    #[test]
    fn local_function_keys_go_both_ways() {
        let ctx = ShortcutContext {
            modifiers: 0x08,
            ..ShortcutContext::default()
        };
        assert_eq!(
            decide_shortcut(&table(), 0xE3, &ctx),
            ShortcutDecision::LocalAndRemote
        );
    }

    #[test]
    fn system_local_wins_over_local_function() {
        let ctx = ShortcutContext::default();
        // Tab 同时出现在 local_function 与 system_local，应判 Blocked。
        assert_eq!(
            decide_shortcut(&table(), 0x2B, &ctx),
            ShortcutDecision::Blocked
        );
    }

    #[test]
    fn default_is_forward() {
        let ctx = ShortcutContext::default();
        assert_eq!(
            decide_shortcut(&table(), 0x04, &ctx),
            ShortcutDecision::SendToRemote
        );
    }
}
