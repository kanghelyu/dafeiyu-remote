// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy-path` 的集成测试：只走公开 API，不建网络、不用 tokio。
//!
//! 这些用例模拟 `dfy-cli` 的完整驱动循环：每 250ms 把当前路径的采样喂进来，
//! 收到 `Action::SwitchTo` 后执行「新连接 + 握手 + `SessionResume`」并回填结果。

use dfy_path::{
    Action, DegradeReason, PathConfig, PathEvent, PathKind, PathMonitor, PathSample, PathStats,
    Phase, SwitchDirection,
};

fn sample(at_ms: u64, rtt_ms: f32, loss_ppm: u32, stall_ms: u64) -> PathSample {
    PathSample {
        rtt_ms,
        loss_ppm,
        recv_bps: 8_000_000,
        stall_ms,
        at_ms,
    }
}

fn good(at_ms: u64) -> PathSample {
    sample(at_ms, 20.0, 0, 0)
}

/// 模拟一次完整的「P2P → 中继 → 回滚 → 再迁 → 回到 P2P」生命周期。
#[test]
fn full_migration_lifecycle_is_deterministic() {
    let mut monitor = PathMonitor::new(PathConfig::default());
    let mut switches = 0u32;

    // 1. 建连后 5 个健康采样建立 RTT 基线（20ms）。
    for i in 0..5u64 {
        assert_eq!(monitor.on_sample(good(1_000 + i * 250)), Action::None);
    }
    assert!(monitor.baseline().is_some());
    assert_eq!(monitor.active(), PathKind::Direct);

    // 2. P2P 持续劣化（60ms > 2×20ms），3 个间隔 500ms 的坏采样后请求切中继。
    assert_eq!(monitor.on_sample(sample(2_250, 60.0, 0, 0)), Action::None);
    assert_eq!(monitor.on_sample(sample(2_750, 60.0, 0, 0)), Action::None);
    assert_eq!(
        monitor.on_sample(sample(3_250, 60.0, 0, 0)),
        Action::SwitchTo(PathKind::Relay)
    );
    // 调用方完成「新连接 + 握手 + SessionResume」并回填（耗时 180ms）。
    assert!(monitor.on_switch_committed(3_750, 180));
    switches += 1;
    assert_eq!(monitor.active(), PathKind::Relay);
    assert!(matches!(
        monitor.state().phase,
        Phase::Observing { since_ms: 3_750 }
    ));

    // 3. 中继立即劣化：观察期内 1.5s 连续 3 个坏采样 → 回滚 P2P + 5s 退避。
    assert_eq!(
        monitor.on_sample_for(PathKind::Relay, sample(4_000, 60.0, 30_000, 0)),
        Action::None
    );
    assert_eq!(
        monitor.on_sample_for(PathKind::Relay, sample(4_250, 60.0, 30_000, 0)),
        Action::None
    );
    assert_eq!(
        monitor.on_sample_for(PathKind::Relay, sample(4_500, 60.0, 30_000, 0)),
        Action::SwitchTo(PathKind::Direct)
    );
    assert_eq!(monitor.active(), PathKind::Direct);
    assert!(matches!(
        monitor.state().phase,
        Phase::Backoff { until_ms: 9_500 }
    ));

    // 4. 退避结束后 P2P 仍然劣化 → 再次切中继（退避期内的采样不计数）。
    assert_eq!(monitor.on_sample(sample(9_500, 60.0, 0, 0)), Action::None);
    assert_eq!(monitor.on_sample(sample(10_000, 60.0, 0, 0)), Action::None);
    assert_eq!(
        monitor.on_sample(sample(10_500, 60.0, 0, 0)),
        Action::SwitchTo(PathKind::Relay)
    );
    assert!(monitor.on_switch_committed(11_000, 150));
    switches += 1;
    assert_eq!(monitor.active(), PathKind::Relay);

    // 5. 观察期结束（5s）后，冷期（30s）内不切回。
    for i in 0..21u64 {
        let action = monitor.on_sample_for(PathKind::Relay, good(11_250 + i * 250));
        assert_ne!(action, Action::SwitchTo(PathKind::Direct));
    }
    assert!(!matches!(monitor.state().phase, Phase::Observing { .. }));
    for i in 0..3u64 {
        assert_eq!(
            monitor.on_sample_for(PathKind::Direct, good(20_000 + i * 1_000)),
            Action::None
        );
    }

    // 6. 冷期过后 P2P 探测连续 5 个好采样 → 切回直连。
    let mut last_action = Action::None;
    for i in 0..6u64 {
        let at = 41_000 + i * 1_000;
        last_action = monitor.on_sample_for(PathKind::Direct, good(at));
        if last_action == Action::SwitchTo(PathKind::Direct) {
            break;
        }
    }
    assert_eq!(last_action, Action::SwitchTo(PathKind::Direct));
    assert!(matches!(
        monitor.state().phase,
        Phase::Switching {
            direction: SwitchDirection::ToDirect,
            ..
        }
    ));
    assert!(monitor.on_switch_committed(45_500, 140));
    switches += 1;

    // 7. 终态与统计。
    assert_eq!(monitor.active(), PathKind::Direct);
    assert_eq!(monitor.stats().current, PathKind::Direct);
    assert_eq!(monitor.stats().switches, switches);
    assert_eq!(monitor.stats().rollbacks, 1);
    assert_eq!(monitor.stats().last_switch_at_ms, Some(45_500));
    assert_eq!(monitor.pending_switch(), None);

    // 事件流覆盖了完整生命周期。
    let events = monitor.drain_events();
    assert!(events
        .iter()
        .any(|event| matches!(event, PathEvent::SwitchCommitted { .. })));
    assert!(monitor.events().is_empty());
}

/// 自定义阈值可以加快触发：2 个坏采样即可切换。
#[test]
fn custom_thresholds_change_sensitivity() {
    let config = PathConfig {
        degrade_bad_samples: 2,
        degrade_min_spacing_ms: 250,
        baseline_samples: 3,
        ..PathConfig::default()
    };
    let mut monitor = PathMonitor::new(config.clone());
    assert!(monitor.config().is_sane());
    for i in 0..3u64 {
        assert_eq!(monitor.on_sample(good(1_000 + i * 250)), Action::None);
    }
    assert_eq!(monitor.on_sample(sample(1_750, 60.0, 0, 0)), Action::None);
    assert_eq!(
        monitor.on_sample(sample(2_000, 60.0, 0, 0)),
        Action::SwitchTo(PathKind::Relay)
    );
}

/// 事件与阶段都实现 `Display`，日志层可直接打印中文描述。
#[test]
fn events_and_phases_are_human_readable() {
    let deteriorated = PathEvent::Deteriorated {
        path: PathKind::Direct,
        reason: DegradeReason::Rtt {
            observed_ms: 90.0,
            baseline_ms: 20.0,
            factor: 2.0,
        },
    };
    let text = deteriorated.to_string();
    assert!(text.contains("P2P"));
    assert!(text.contains("90.0ms"));
    assert!(PathEvent::RolledBack {
        to: PathKind::Relay
    }
    .to_string()
    .contains("Relay"));
    let switching = Phase::Switching {
        direction: SwitchDirection::ToDirect,
        started_at_ms: 5_000,
    };
    assert!(switching.to_string().contains("Relay → P2P"));
}

/// 非法采样/非法配置不会 panic，统计仍然自洽。
#[test]
fn garbage_input_stays_bounded() {
    let mut monitor = PathMonitor::new(PathConfig {
        window_capacity: 0,
        ewma_alpha: f32::NAN,
        ..PathConfig::default()
    });
    assert_eq!(monitor.window(PathKind::Direct).capacity(), 1);

    let garbage = [
        sample(0, f32::NAN, u32::MAX, u64::MAX),
        sample(10, f32::INFINITY, 0, 0),
        sample(5, -100.0, 1_500_000, 0), // 时间倒退 + 负 RTT + 丢包越界
        PathSample {
            rtt_ms: 20.0,
            loss_ppm: 0,
            recv_bps: u64::MAX,
            stall_ms: 0,
            at_ms: 1_000,
        },
    ];
    for item in garbage {
        assert_eq!(monitor.on_sample(item), Action::None);
    }
    assert_eq!(monitor.stats().samples, 4);
    assert!(monitor.stats().switches == 0);
    // 窗口容量 1：只保留最后一条，且值已被清洗。
    let window = monitor.window(PathKind::Direct);
    assert_eq!(window.len(), 1);
    assert_eq!(window.latest().unwrap().at_ms, 1_000);
    assert_eq!(window.latest().unwrap().recv_bps, u64::MAX);
}

/// 统计快照是 `Copy` 数据，可安全地周期性上报。
#[test]
fn stats_snapshot_is_copy_and_updates() {
    let monitor = PathMonitor::new(PathConfig::default());
    let before: PathStats = *monitor.stats();
    let snapshot = before;
    assert_eq!(snapshot.current, PathKind::Direct);
    assert!(before.samples == 0 && snapshot.samples == 0);
}
