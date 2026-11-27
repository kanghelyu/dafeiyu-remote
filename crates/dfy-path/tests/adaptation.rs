// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

use dfy_path::{
    AdaptationConfig, AdaptationController, AdaptationDirection, AdaptationReason,
    AdaptationResolution, PathSample,
};

fn sample(at_ms: u64, rtt_ms: f32, loss_ppm: u32, recv_bps: u64, stall_ms: u64) -> PathSample {
    PathSample {
        rtt_ms,
        loss_ppm,
        recv_bps,
        stall_ms,
        at_ms,
    }
}

fn good(at_ms: u64) -> PathSample {
    sample(at_ms, 20.0, 0, 100_000_000, 0)
}

fn controller(config: AdaptationConfig) -> AdaptationController {
    AdaptationController::new(
        config,
        AdaptationResolution {
            width: 1280,
            height: 720,
        },
        60,
        6000,
    )
    .unwrap()
}

#[test]
fn sustained_congestion_lowers_resolution_fps_and_bitrate() {
    let mut controller = controller(AdaptationConfig::default());
    let bad = sample(0, 300.0, 0, 8_000_000, 0);
    let first = controller.on_sample(bad);
    assert!(!first.changed);
    assert_eq!(first.reason, AdaptationReason::WaitingForCongestion);

    let second = controller.on_sample(PathSample { at_ms: 1000, ..bad });
    assert!(second.changed);
    assert_eq!(second.direction, AdaptationDirection::Down);
    assert_eq!(second.reason, AdaptationReason::Congested);
    assert_eq!(second.resolution.width, 1024);
    assert_eq!(second.resolution.height, 576);
    assert_eq!(second.fps, 48);
    assert_eq!(second.bitrate_kbps, 4800);
}

#[test]
fn sustained_recovery_raises_quality_within_bounds() {
    let config = AdaptationConfig {
        max_width: 1280,
        max_height: 720,
        max_fps: 120,
        max_bitrate_kbps: 8000,
        congestion_samples: 1,
        recovery_samples: 3,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    };
    let mut controller = AdaptationController::new(
        config,
        AdaptationResolution {
            width: 1280,
            height: 720,
        },
        60,
        6000,
    )
    .unwrap();
    let down = controller.on_sample(sample(0, 300.0, 0, 8_000_000, 0));
    assert_eq!(down.direction, AdaptationDirection::Down);
    assert_eq!((down.resolution.width, down.resolution.height), (1024, 576));

    assert!(!controller.on_sample(good(1000)).changed);
    assert!(!controller.on_sample(good(2000)).changed);
    let recovered = controller.on_sample(good(3000));
    assert!(recovered.changed);
    assert_eq!(recovered.direction, AdaptationDirection::Up);
    assert_eq!(recovered.resolution.width, 1280);
    assert_eq!(recovered.resolution.height, 720);
    assert_eq!(recovered.fps, 60);
    assert_eq!(recovered.bitrate_kbps, 6000);
}

#[test]
fn degraded_initial_quality_recovers_to_a_separate_ceiling() {
    let config = AdaptationConfig {
        max_width: 1920,
        max_height: 1080,
        max_fps: 120,
        max_bitrate_kbps: 20_000,
        congestion_samples: 1,
        recovery_samples: 1,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    };
    let mut controller = AdaptationController::new_with_ceiling(
        config,
        AdaptationResolution {
            width: 1024,
            height: 576,
        },
        AdaptationResolution {
            width: 1920,
            height: 1080,
        },
        48,
        4800,
    )
    .unwrap();
    assert_eq!(controller.current().resolution.width, 1024);
    let recovered = controller.on_sample(good(1000));
    assert_eq!(recovered.direction, AdaptationDirection::Up);
    assert_eq!(
        (recovered.resolution.width, recovered.resolution.height),
        (1280, 720)
    );
    assert_eq!(recovered.fps, 60);
    assert_eq!(recovered.bitrate_kbps, 6000);
}

#[test]
fn oscillation_does_not_flip_quality_on_alternating_samples() {
    let mut controller = controller(AdaptationConfig::default());
    let bad = sample(0, 400.0, 0, 8_000_000, 0);
    assert!(!controller.on_sample(bad).changed);
    let down = controller.on_sample(PathSample { at_ms: 1000, ..bad });
    assert_eq!(down.direction, AdaptationDirection::Down);

    for i in 0..20 {
        let at_ms = 2000 + i * 500;
        let result = if i % 2 == 0 {
            controller.on_sample(good(at_ms))
        } else {
            controller.on_sample(PathSample { at_ms, ..bad })
        };
        assert!(
            !result.changed,
            "alternating network samples must not change quality"
        );
        assert_eq!(result.direction, AdaptationDirection::Hold);
        assert_eq!(result.resolution, down.resolution);
        assert_eq!(result.fps, down.fps);
        assert_eq!(result.bitrate_kbps, down.bitrate_kbps);
    }
}

#[test]
fn one_hundred_twenty_fps_is_supported_and_not_silently_capped() {
    let config = AdaptationConfig {
        max_fps: 120,
        ..AdaptationConfig::default()
    };
    let mut controller = AdaptationController::new(
        config,
        AdaptationResolution {
            width: 1920,
            height: 1080,
        },
        120,
        20_000,
    )
    .unwrap();
    for i in 0..20 {
        let result = controller.on_sample(good(i * 250));
        assert!(!result.changed);
        assert_eq!(result.fps, 120);
        if i < 4 {
            assert_eq!(result.reason, AdaptationReason::WaitingForRecovery);
        } else {
            assert_eq!(result.reason, AdaptationReason::AtMaximum);
        }
    }
}

#[test]
fn repeated_congestion_and_recovery_hit_both_bounds() {
    let config = AdaptationConfig {
        min_width: 640,
        min_height: 360,
        max_width: 1920,
        max_height: 1080,
        min_fps: 15,
        max_fps: 120,
        min_bitrate_kbps: 500,
        max_bitrate_kbps: 20_000,
        congestion_samples: 1,
        recovery_samples: 1,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    };
    let mut controller = AdaptationController::new(
        config,
        AdaptationResolution {
            width: 1920,
            height: 1080,
        },
        120,
        20_000,
    )
    .unwrap();
    let bad = sample(0, 500.0, 100_000, 0, 5000);
    for i in 1..=20 {
        controller.on_sample(PathSample {
            at_ms: i * 1000,
            ..bad
        });
    }
    let low = controller.current();
    assert_eq!((low.resolution.width, low.resolution.height), (640, 360));
    assert_eq!(low.fps, 15);
    assert_eq!(low.bitrate_kbps, 500);

    for i in 21..=40 {
        controller.on_sample(good(i * 1000));
    }
    let high = controller.current();
    assert_eq!(
        (high.resolution.width, high.resolution.height),
        (1920, 1080)
    );
    assert_eq!(high.fps, 120);
    assert_eq!(high.bitrate_kbps, 20_000);
}

#[test]
fn each_network_congestion_input_can_trigger_downgrade() {
    let cases = [
        sample(0, 151.0, 0, 100_000_000, 0),
        sample(0, 20.0, 30_001, 100_000_000, 0),
        sample(0, 20.0, 0, 100_000_000, 1001),
    ];
    for bad in cases {
        let mut controller = controller(AdaptationConfig::default());
        controller.on_sample(bad);
        let result = controller.on_sample(PathSample { at_ms: 1000, ..bad });
        assert_eq!(result.direction, AdaptationDirection::Down);
    }
}

#[test]
fn low_vbr_goodput_alone_does_not_trigger_downgrade() {
    let mut controller = controller(AdaptationConfig {
        max_width: 1280,
        max_height: 720,
        max_fps: 60,
        max_bitrate_kbps: 6000,
        ..AdaptationConfig::default()
    });
    let healthy = sample(0, 1.0, 0, 200_000, 0);
    for at_ms in [0, 500, 1000, 1500, 2000, 2500, 3000] {
        let result = controller.on_sample(PathSample { at_ms, ..healthy });
        assert!(!result.changed);
        assert_eq!(result.direction, AdaptationDirection::Hold);
        assert_eq!(result.fps, 60);
        assert_eq!(result.resolution.width, 1280);
        assert_eq!(result.bitrate_kbps, 6000);
    }
}

#[test]
fn recovery_requires_lower_loss_rtt_and_stall() {
    let mut controller = controller(AdaptationConfig {
        recovery_samples: 1,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    });
    let borderline = [
        sample(0, 81.0, 0, 100_000_000, 0),
        sample(0, 20.0, 5001, 100_000_000, 0),
        sample(0, 20.0, 0, 100_000_000, 501),
    ];
    for not_good in borderline {
        let result = controller.on_sample(not_good);
        assert!(!result.changed);
        assert_ne!(result.direction, AdaptationDirection::Up);
    }
}

#[test]
fn measured_subsecond_encoder_gaps_allow_recovery_without_ignoring_real_stalls() {
    let mut controller = controller(AdaptationConfig {
        congestion_samples: 1,
        recovery_samples: 1,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    });
    assert_eq!(
        controller
            .on_sample(sample(0, 300.0, 0, 8_000_000, 0))
            .direction,
        AdaptationDirection::Down
    );
    let recovered = controller.on_sample(sample(1000, 2.0, 0, 200_000, 269));
    assert_eq!(recovered.direction, AdaptationDirection::Up);
    assert_eq!(recovered.resolution.width, 1280);
    assert_eq!(recovered.fps, 60);

    assert_eq!(
        controller
            .on_sample(sample(2000, 300.0, 0, 8_000_000, 0))
            .direction,
        AdaptationDirection::Down
    );
    let stalled = controller.on_sample(sample(3000, 2.0, 0, 200_000, 501));
    assert!(!stalled.changed);
    assert_eq!(stalled.direction, AdaptationDirection::Hold);
    assert_eq!(stalled.reason, AdaptationReason::WaitingForRecovery);
}

#[test]
fn recovery_accepts_low_vbr_goodput_after_congestion_clears() {
    let mut controller = controller(AdaptationConfig {
        congestion_samples: 1,
        recovery_samples: 2,
        min_change_interval_ms: 0,
        recovery_cooldown_ms: 0,
        ..AdaptationConfig::default()
    });
    let congested = sample(0, 300.0, 0, 200_000, 0);
    assert_eq!(
        controller.on_sample(congested).direction,
        AdaptationDirection::Down
    );
    let healthy = sample(1000, 1.0, 0, 200_000, 0);
    assert!(!controller.on_sample(healthy).changed);
    let recovered = controller.on_sample(PathSample {
        at_ms: 2000,
        ..healthy
    });
    assert_eq!(recovered.direction, AdaptationDirection::Up);
    assert_eq!(recovered.resolution.width, 1280);
    assert_eq!(recovered.fps, 60);
}

#[test]
fn stale_samples_are_ignored_and_invalid_configs_fail_explicitly() {
    let mut controller = controller(AdaptationConfig::default());
    controller.on_sample(good(100));
    let stale = controller.on_sample(good(100));
    assert!(!stale.changed);
    assert_eq!(stale.reason, AdaptationReason::StaleSample);
    let backwards = controller.on_sample(good(99));
    assert_eq!(backwards.reason, AdaptationReason::StaleSample);

    let bad_fps = AdaptationConfig {
        max_fps: 121,
        ..AdaptationConfig::default()
    };
    assert!(bad_fps.validate().is_err());
    let no_hysteresis = AdaptationConfig {
        recovery_rtt_ms: 200.0,
        ..AdaptationConfig::default()
    };
    assert!(no_hysteresis.validate().is_err());
    let bad_bounds = AdaptationConfig {
        min_width: 1280,
        max_width: 640,
        ..AdaptationConfig::default()
    };
    assert!(bad_bounds.validate().is_err());
}

#[test]
fn portrait_aspect_ratio_is_preserved_by_bounds_and_scaling() {
    let mut controller = AdaptationController::new(
        AdaptationConfig::default(),
        AdaptationResolution {
            width: 720,
            height: 1280,
        },
        60,
        6000,
    )
    .unwrap();
    let initial = controller.current();
    assert_eq!(
        (initial.resolution.width, initial.resolution.height),
        (608, 1080)
    );
    let bad = sample(0, 500.0, 0, 8_000_000, 0);
    controller.on_sample(bad);
    let down = controller.on_sample(PathSample { at_ms: 1000, ..bad });
    let ratio_error =
        (i64::from(down.resolution.width) * 1280 - i64::from(down.resolution.height) * 720).abs();
    assert!(
        ratio_error <= 1280,
        "scaled portrait aspect drift: {down:?}"
    );
    assert!(
        down.resolution.width <= initial.resolution.width
            && down.resolution.height <= initial.resolution.height
    );
}
