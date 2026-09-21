use std::{
    sync::{Arc, Barrier},
    thread,
};

use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::{
    domain::{Availability, LimitKind, QuotaSnapshotInput, normalize_rows},
    notify::{Notifier, TerminalNotifier, deliver},
};

const NOW: i64 = 1_700_000_000_000;

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

fn fixture() -> (TempDir, Storage) {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    (temp, storage)
}

fn snapshot(observed_at: i64, used: f64, reset: Option<i64>, quality: Quality) -> QuotaSnapshot {
    QuotaSnapshot::new(QuotaSnapshotInput {
        provider: Provider::Codex,
        scope_key: "subscription".into(),
        window_kind: WindowKind::Rolling5h,
        limit_kind: LimitKind::Limited,
        window_duration_seconds: Some(18_000),
        used_percent: used,
        resets_at: reset,
        observed_at,
        availability: Availability::Allowed,
        source_version: None,
        quality,
        source_sequence: Uuid::new_v4(),
    })
    .unwrap()
}

fn copilot_snapshot(observed_at: i64, used: f64, reset: Option<i64>) -> QuotaSnapshot {
    QuotaSnapshot::new(copilot_input(observed_at, used, reset, LimitKind::Limited)).unwrap()
}

fn antigravity_snapshot(
    scope_key: &str,
    observed_at: i64,
    used: f64,
    reset: Option<i64>,
) -> QuotaSnapshot {
    QuotaSnapshot::new(QuotaSnapshotInput {
        provider: Provider::GoogleAntigravity,
        scope_key: scope_key.into(),
        window_kind: WindowKind::Other,
        limit_kind: LimitKind::Limited,
        window_duration_seconds: None,
        used_percent: used,
        resets_at: reset,
        observed_at,
        availability: if used >= 100.0 {
            Availability::Blocked
        } else {
            Availability::Allowed
        },
        source_version: Some("1.2.3".into()),
        quality: Quality::Fresh,
        source_sequence: Uuid::new_v4(),
    })
    .unwrap()
}

fn copilot_input(
    observed_at: i64,
    used: f64,
    reset: Option<i64>,
    limit_kind: LimitKind,
) -> QuotaSnapshotInput {
    QuotaSnapshotInput {
        provider: Provider::GitHubCopilot,
        scope_key: "premium_interactions".into(),
        window_kind: WindowKind::Monthly,
        limit_kind,
        window_duration_seconds: None,
        used_percent: used,
        resets_at: reset,
        observed_at,
        availability: Availability::Allowed,
        source_version: None,
        quality: Quality::Fresh,
        source_sequence: Uuid::new_v4(),
    }
}

fn process(storage: &Storage, row: QuotaSnapshot, now: i64) -> Vec<AlertEvent> {
    storage
        .insert_snapshots(std::slice::from_ref(&row))
        .unwrap();
    AlertEngine::new(AppConfig::default())
        .process(storage, &[row], &FixedClock(now))
        .unwrap()
}

fn process_with_config(
    storage: &Storage,
    row: QuotaSnapshot,
    now: i64,
    config: AppConfig,
) -> Vec<AlertEvent> {
    storage
        .insert_snapshots(std::slice::from_ref(&row))
        .unwrap();
    AlertEngine::new(config)
        .process(storage, &[row], &FixedClock(now))
        .unwrap()
}

#[test]
fn alerts_low_remaining_only_on_a_same_cycle_crossing() {
    let (_temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + 3_600);
    assert!(
        process(
            &storage,
            snapshot(NOW - 2_000, 70.0, reset, Quality::Fresh),
            NOW
        )
        .is_empty()
    );
    let events = process(
        &storage,
        snapshot(NOW - 1_000, 80.0, reset, Quality::Fresh),
        NOW,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AlertKind::LowRemaining);

    let events = process(&storage, snapshot(NOW, 90.0, reset, Quality::Fresh), NOW);
    assert!(
        !events
            .iter()
            .any(|event| event.kind == AlertKind::LowRemaining)
    );
}

#[test]
fn alerts_initial_low_and_new_cycle_low_do_not_count_as_crossings() {
    let (_temp, storage) = fixture();
    let first_reset = Some(NOW / 1_000 + 3_600);
    assert!(
        process(
            &storage,
            snapshot(NOW - 2_000, 90.0, first_reset, Quality::Fresh),
            NOW
        )
        .is_empty()
    );
    let second_reset = Some(NOW / 1_000 + 7_200);
    let events = process(
        &storage,
        snapshot(NOW, 85.0, second_reset, Quality::Fresh),
        NOW,
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == AlertKind::LowRemaining)
    );
}

#[test]
fn alerts_reset_soon_uses_inclusive_horizon_and_future_boundary() {
    let (_temp, storage) = fixture();
    let horizon = AppConfig::default().reset_soon_seconds as i64;
    let outside = snapshot(
        NOW - 2_000,
        10.0,
        Some(NOW / 1_000 + horizon + 1),
        Quality::Fresh,
    );
    assert!(process(&storage, outside, NOW).is_empty());
    let boundary = snapshot(
        NOW - 1_000,
        11.0,
        Some(NOW / 1_000 + horizon),
        Quality::Fresh,
    );
    let events = process(&storage, boundary, NOW);
    assert!(
        events
            .iter()
            .any(|event| event.kind == AlertKind::ResetSoon)
    );

    let expired = snapshot(NOW, 12.0, Some(NOW / 1_000), Quality::Fresh);
    assert!(process(&storage, expired, NOW).is_empty());
}

#[test]
fn reset_soon_message_uses_firing_time_for_relative_wording() {
    let reset = NOW / 1_000 + 125;
    let event = AlertEvent {
        provider: Provider::Codex,
        scope_key: "subscription".into(),
        window_kind: WindowKind::Rolling5h,
        kind: AlertKind::ResetSoon,
        cycle_key: reset.to_string(),
        fired_at: NOW,
        remaining_percent: 50.0,
        resets_at: Some(reset),
    };

    let message = event.message();
    assert!(message.contains("resets in 2m 5s"));
    assert!(!message.contains(&reset.to_string()));

    let mut output = Vec::new();
    TerminalNotifier::new(&mut output, false)
        .notify(&event)
        .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("resets in 2m 5s"));
    assert!(!output.contains(&reset.to_string()));
}

#[test]
fn alerts_reset_observed_requires_advanced_reset_and_lower_usage() {
    let (_temp, storage) = fixture();
    let old = NOW / 1_000 + 300;
    process(
        &storage,
        snapshot(NOW - 1_000, 75.0, Some(old), Quality::Fresh),
        NOW,
    );
    let events = process(
        &storage,
        snapshot(NOW, 20.0, Some(old + 3_600), Quality::Fresh),
        NOW,
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == AlertKind::ResetObserved)
    );

    let no_drop = process(
        &storage,
        snapshot(NOW + 1, 25.0, Some(old + 7_200), Quality::Fresh),
        NOW + 1,
    );
    assert!(
        !no_drop
            .iter()
            .any(|event| event.kind == AlertKind::ResetObserved)
    );
}

#[test]
fn alerts_wall_clock_passage_never_proves_a_reset() {
    let (_temp, storage) = fixture();
    let reset = NOW / 1_000 + 1;
    process(
        &storage,
        snapshot(NOW, 80.0, Some(reset), Quality::Fresh),
        NOW,
    );
    let events = process(
        &storage,
        snapshot(NOW + 2_000, 10.0, Some(reset), Quality::Fresh),
        NOW + 2_000,
    );
    assert!(events.is_empty());
}

#[test]
fn alerts_partial_and_stale_observations_are_suppressed() {
    let (_temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + 300);
    process(
        &storage,
        snapshot(NOW - 5_000, 70.0, reset, Quality::Fresh),
        NOW,
    );
    assert!(
        process(
            &storage,
            snapshot(NOW - 1_000, 90.0, reset, Quality::Partial),
            NOW
        )
        .is_empty()
    );

    let stale = snapshot(NOW - 181_000, 90.0, reset, Quality::Fresh);
    assert!(process(&storage, stale, NOW).is_empty());
}

#[test]
fn copilot_alert_freshness_uses_the_configured_polling_cadence() {
    let (_temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + 3_600);
    let previous = copilot_snapshot(NOW - 1_000_000, 70.0, reset);
    storage.insert_snapshots(&[previous]).unwrap();
    let current = copilot_snapshot(NOW - 900_000, 85.0, reset);
    storage
        .insert_snapshots(std::slice::from_ref(&current))
        .unwrap();

    let default_events = AlertEngine::new(AppConfig::default())
        .process(&storage, std::slice::from_ref(&current), &FixedClock(NOW))
        .unwrap();
    assert!(default_events.is_empty());

    let events = AlertEngine::new(AppConfig {
        copilot_refresh_seconds: 600,
        ..AppConfig::default()
    })
    .process(&storage, &[current], &FixedClock(NOW))
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == AlertKind::LowRemaining)
    );
}

#[test]
fn finite_copilot_alerts_cross_threshold_and_dedupe_per_cycle() {
    let (_temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + 3_600);
    assert!(process(&storage, copilot_snapshot(NOW - 2_000, 70.0, reset), NOW).is_empty());

    let current = copilot_snapshot(NOW - 1_000, 85.0, reset);
    let events = process(&storage, current.clone(), NOW);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == AlertKind::LowRemaining)
            .count(),
        1
    );
    assert!(
        AlertEngine::new(AppConfig::default())
            .process(&storage, &[current], &FixedClock(NOW))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn finite_copilot_reset_soon_claim_survives_restart() {
    let (temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + AppConfig::default().reset_soon_seconds as i64);
    let current = copilot_snapshot(NOW, 25.0, reset);
    let events = process(&storage, current.clone(), NOW);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == AlertKind::ResetSoon)
            .count(),
        1
    );

    let reopened = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    assert!(
        AlertEngine::new(AppConfig::default())
            .process(&reopened, &[current], &FixedClock(NOW))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn antigravity_low_and_reset_transitions_dedupe_by_cycle() {
    let (_temp, storage) = fixture();
    let first_reset = NOW / 1_000 + 3_600;
    assert!(
        process(
            &storage,
            antigravity_snapshot("gemini-weekly", NOW - 2_000, 70.0, Some(first_reset)),
            NOW,
        )
        .is_empty()
    );

    let low = antigravity_snapshot("gemini-weekly", NOW - 1_000, 85.0, Some(first_reset));
    let events = process(&storage, low.clone(), NOW);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == AlertKind::LowRemaining)
            .count(),
        1
    );
    assert!(events[0].message().contains("Gemini weekly"));
    assert!(!events[0].message().contains("gemini-weekly"));
    assert!(
        AlertEngine::new(AppConfig::default())
            .process(&storage, &[low], &FixedClock(NOW))
            .unwrap()
            .is_empty()
    );

    let events = process(
        &storage,
        antigravity_snapshot("gemini-weekly", NOW, 20.0, Some(first_reset + 3_600)),
        NOW,
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == AlertKind::ResetObserved)
    );
}

#[test]
fn antigravity_reset_soon_uses_existing_cycle_dedupe() {
    let (temp, storage) = fixture();
    let reset = NOW / 1_000 + AppConfig::default().reset_soon_seconds as i64;
    let current = antigravity_snapshot("pro_model", NOW, 25.0, Some(reset));
    let events = process(&storage, current.clone(), NOW);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == AlertKind::ResetSoon)
            .count(),
        1
    );

    let reopened = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    assert!(
        AlertEngine::new(AppConfig::default())
            .process(&reopened, &[current], &FixedClock(NOW))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn antigravity_alert_freshness_uses_exact_configured_boundary() {
    let config = AppConfig {
        antigravity_stale_after_seconds: 120,
        ..AppConfig::default()
    };
    let reset = Some(NOW / 1_000 + 3_600);

    let (_fresh_temp, fresh_storage) = fixture();
    fresh_storage
        .insert_snapshots(&[antigravity_snapshot(
            "pro_model",
            NOW - 121_000,
            70.0,
            reset,
        )])
        .unwrap();
    let boundary_events = process_with_config(
        &fresh_storage,
        antigravity_snapshot("pro_model", NOW - 120_000, 85.0, reset),
        NOW,
        config.clone(),
    );
    assert!(
        boundary_events
            .iter()
            .any(|event| event.kind == AlertKind::LowRemaining)
    );

    let (_stale_temp, stale_storage) = fixture();
    stale_storage
        .insert_snapshots(&[antigravity_snapshot(
            "pro_model",
            NOW - 121_002,
            70.0,
            reset,
        )])
        .unwrap();
    let stale_events = process_with_config(
        &stale_storage,
        antigravity_snapshot("pro_model", NOW - 120_001, 85.0, reset),
        NOW,
        config,
    );
    assert!(stale_events.is_empty());
}

#[test]
fn unlimited_and_rejected_copilot_rows_never_emit_threshold_alerts() {
    let (_temp, storage) = fixture();
    let reset = Some(NOW / 1_000 + 60);
    let batch = normalize_rows([
        copilot_input(NOW - 1_000, 101.0, reset, LimitKind::Limited),
        copilot_input(NOW, 0.0, reset, LimitKind::Unlimited),
    ]);
    assert_eq!(batch.rejected.len(), 1);
    assert_eq!(batch.accepted.len(), 1);

    storage.insert_snapshots(&batch.accepted).unwrap();
    let events = AlertEngine::new(AppConfig::default())
        .process(&storage, &batch.accepted, &FixedClock(NOW))
        .unwrap();
    assert!(events.is_empty());
    assert!(storage.recent_alerts(10).unwrap().is_empty());
}

#[test]
fn alerts_claims_survive_restart_and_allow_distinct_cycles() {
    let (temp, storage) = fixture();
    let first_reset = NOW / 1_000 + 300;
    let first = snapshot(NOW, 10.0, Some(first_reset), Quality::Fresh);
    assert_eq!(process(&storage, first.clone(), NOW).len(), 1);
    let reopened = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    assert!(
        AlertEngine::new(AppConfig::default())
            .process(&reopened, &[first], &FixedClock(NOW))
            .unwrap()
            .is_empty()
    );

    let second = snapshot(NOW + 1, 5.0, Some(first_reset + 3_600), Quality::Fresh);
    let events = process(&reopened, second, NOW + 1);
    assert!(
        events
            .iter()
            .any(|event| event.kind == AlertKind::ResetObserved)
    );
}

#[test]
fn alerts_claim_is_atomic_under_concurrent_writers() {
    let (_temp, storage) = fixture();
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let storage = storage.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                storage
                    .claim_alert(
                        Provider::Codex,
                        "subscription",
                        WindowKind::Rolling5h,
                        "reset_soon",
                        "cycle-1",
                        NOW,
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|claimed| *claimed)
        .count();
    assert_eq!(winners, 1);
}

#[derive(Default)]
struct CountingNotifier {
    calls: usize,
    fail: bool,
}

impl Notifier for CountingNotifier {
    fn notify(&mut self, _event: &AlertEvent) -> Result<(), crate::notify::NotifyError> {
        self.calls += 1;
        if self.fail {
            Err(crate::notify::NotifyError)
        } else {
            Ok(())
        }
    }
}

#[test]
fn alerts_native_failure_keeps_terminal_delivery_and_is_nonfatal() {
    let event = AlertEvent {
        provider: Provider::Claude,
        scope_key: "subscription".into(),
        window_kind: WindowKind::Rolling7d,
        kind: AlertKind::ResetSoon,
        cycle_key: "1".into(),
        fired_at: NOW,
        remaining_percent: 50.0,
        resets_at: Some(NOW / 1_000 + 60),
    };
    let mut terminal = CountingNotifier::default();
    let mut native = CountingNotifier {
        fail: true,
        ..CountingNotifier::default()
    };
    let report = deliver(&[event], &mut terminal, Some(&mut native));
    assert_eq!(terminal.calls, 1);
    assert_eq!(report.native_failures, 1);
    assert_eq!(report.terminal_failures, 0);
}

#[test]
fn alerts_history_is_bounded_and_prunable_without_bodies() {
    let (_temp, storage) = fixture();
    for cycle in 1..=3 {
        storage
            .claim_alert(
                Provider::Codex,
                "subscription",
                WindowKind::Rolling5h,
                "reset_soon",
                &cycle.to_string(),
                cycle,
            )
            .unwrap();
    }
    let history = storage.recent_alerts(2).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].cycle_key, "3");
    assert_eq!(storage.prune_alerts_before(3).unwrap(), 2);
    assert_eq!(storage.recent_alerts(10).unwrap().len(), 1);
    assert!(matches!(
        storage.recent_alerts(0),
        Err(StorageError::InvalidHistoryLimit(0))
    ));
}

#[test]
fn alerts_terminal_output_bells_only_when_requested() {
    let event = AlertEvent {
        provider: Provider::Codex,
        scope_key: "subscription".into(),
        window_kind: WindowKind::Rolling5h,
        kind: AlertKind::LowRemaining,
        cycle_key: "cycle".into(),
        fired_at: NOW,
        remaining_percent: 20.0,
        resets_at: Some(NOW / 1_000 + 3_600),
    };
    let mut quiet_output = Vec::new();
    TerminalNotifier::new(&mut quiet_output, false)
        .notify(&event)
        .unwrap();
    assert!(!quiet_output.contains(&7));
    assert!(
        String::from_utf8(quiet_output)
            .unwrap()
            .contains("20.0% remaining")
    );

    let mut bell_output = Vec::new();
    TerminalNotifier::new(&mut bell_output, true)
        .notify(&event)
        .unwrap();
    assert_eq!(bell_output.first(), Some(&7));
}

#[test]
fn alerts_recent_window_history_is_bounded() {
    let (_temp, storage) = fixture();
    for index in 1..=3 {
        storage
            .insert_snapshots(&[snapshot(
                NOW + index,
                index as f64,
                Some(NOW / 1_000 + 3_600),
                Quality::Fresh,
            )])
            .unwrap();
    }
    let rows = storage
        .recent_window_history(Provider::Codex, "subscription", WindowKind::Rolling5h, 2)
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].snapshot.as_input().observed_at > rows[1].snapshot.as_input().observed_at);
}
