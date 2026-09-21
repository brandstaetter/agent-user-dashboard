use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use tempfile::tempdir;
use uuid::Uuid;

use super::*;
use crate::{
    domain::{Availability, LimitKind, Quality, QuotaSnapshotInput, WindowKind},
    providers::github_copilot::{CopilotError, CopilotObservation},
};

#[derive(Clone, Copy)]
struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

struct FakeProbe {
    results: Mutex<VecDeque<Result<CodexObservation, CodexError>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    called: Option<mpsc::Sender<()>>,
}

struct FakeCopilotProbe {
    results: Mutex<VecDeque<Result<CopilotObservation, CopilotError>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    called: Option<mpsc::Sender<()>>,
}

impl FakeCopilotProbe {
    fn new(results: Vec<Result<CopilotObservation, CopilotError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            called: None,
        }
    }
}

impl CopilotProbe for FakeCopilotProbe {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        if let Some(called) = &self.called {
            let _ = called.send(());
        }
        let mut result = self
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(copilot_observation(observed_at, 25.0)));
        if let Ok(value) = &mut result {
            for row in &mut value.snapshots {
                row.observed_at = observed_at;
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

impl FakeProbe {
    fn new(results: Vec<Result<CodexObservation, CodexError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            called: None,
        }
    }
}

impl CodexProbe for FakeProbe {
    fn read(&self, observed_at: i64) -> Result<CodexObservation, CodexError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        if let Some(called) = &self.called {
            let _ = called.send(());
        }
        let mut result = self
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(observation(observed_at, 25.0)));
        if let Ok(value) = &mut result {
            for row in &mut value.snapshots {
                row.observed_at = observed_at;
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

struct StepWait {
    waits: AtomicUsize,
    stop_after: usize,
    durations: Mutex<Vec<Duration>>,
}

impl PollWait for StepWait {
    fn wait(&self, duration: Duration) -> bool {
        self.durations.lock().unwrap().push(duration);
        self.waits.fetch_add(1, Ordering::SeqCst) + 1 >= self.stop_after
    }

    fn stopped(&self) -> bool {
        false
    }
}

fn observation(observed_at: i64, used_percent: f64) -> CodexObservation {
    CodexObservation {
        snapshots: vec![QuotaSnapshotInput {
            provider: Provider::Codex,
            scope_key: "subscription".into(),
            window_kind: WindowKind::Rolling5h,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: Some(18_000),
            used_percent,
            resets_at: Some(1_800_000_000),
            observed_at,
            availability: Availability::Allowed,
            source_version: Some("fake".into()),
            quality: Quality::Fresh,
            source_sequence: Uuid::new_v4(),
        }],
        rejected_windows: 0,
    }
}

fn copilot_observation(observed_at: i64, used_percent: f64) -> CopilotObservation {
    CopilotObservation {
        snapshots: vec![QuotaSnapshotInput {
            provider: Provider::GitHubCopilot,
            scope_key: "premium_interactions".into(),
            window_kind: WindowKind::Monthly,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: None,
            used_percent,
            resets_at: Some(1_800_000_000),
            observed_at,
            availability: Availability::Allowed,
            source_version: Some("fake".into()),
            quality: Quality::Fresh,
            source_sequence: Uuid::new_v4(),
        }],
        rejected_windows: 0,
    }
}

#[test]
fn dashboard_polling_uses_independent_intervals_and_is_single_flight() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let probe = FakeProbe::new(vec![]);
    let wait = StepWait {
        waits: AtomicUsize::new(0),
        stop_after: 3,
        durations: Mutex::new(Vec::new()),
    };
    let config = AppConfig {
        codex_refresh_seconds: 30,
        ..AppConfig::default()
    };
    run_codex_poll_loop(&storage, &probe, &config, &FixedClock(2_000), &wait);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
    assert_eq!(probe.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(
        wait.durations.lock().unwrap().as_slice(),
        &[Duration::from_secs(30); 3]
    );

    let copilot_probe = FakeCopilotProbe::new(vec![]);
    let copilot_wait = StepWait {
        waits: AtomicUsize::new(0),
        stop_after: 2,
        durations: Mutex::new(Vec::new()),
    };
    let config = AppConfig {
        codex_refresh_seconds: 30,
        copilot_refresh_seconds: 600,
        ..AppConfig::default()
    };
    run_copilot_poll_loop(
        &storage,
        &copilot_probe,
        &config,
        &FixedClock(3_000),
        &copilot_wait,
    );
    assert_eq!(copilot_probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(copilot_probe.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(
        copilot_wait.durations.lock().unwrap().as_slice(),
        &[Duration::from_secs(600); 2]
    );
}

#[test]
fn dashboard_polling_failure_preserves_last_good_snapshot_and_sanitizes_state() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let good = FakeProbe::new(vec![Ok(observation(1_000, 22.0))]);
    poll_codex_once(&storage, &good, &AppConfig::default(), &FixedClock(1_000));
    let failed = FakeProbe::new(vec![Err(CodexError::Rpc {
        method: "account/rateLimits/read",
        request_id: 2,
    })]);
    poll_codex_once(&storage, &failed, &AppConfig::default(), &FixedClock(2_000));
    let latest = storage.latest_snapshots().unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].snapshot.as_input().used_percent, 22.0);
    let state = storage.provider_state(Provider::Codex).unwrap().unwrap();
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.last_error_class.as_deref(), Some("rpc"));
}

#[test]
fn dashboard_polling_evaluates_and_persists_alerts() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let prior =
        crate::domain::QuotaSnapshot::new(observation(1_000, 70.0).snapshots.remove(0)).unwrap();
    storage.insert_snapshots(&[prior]).unwrap();
    let probe = FakeProbe::new(vec![Ok(observation(2_000, 85.0))]);
    poll_codex_once(&storage, &probe, &AppConfig::default(), &FixedClock(2_000));
    let alerts = storage.recent_alerts(10).unwrap();
    assert!(
        alerts
            .iter()
            .any(|alert| alert.alert_kind == "low_remaining")
    );
}

#[test]
fn dashboard_polling_shutdown_wakes_and_joins_without_waiting_for_interval() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let (called_tx, called_rx) = mpsc::channel();
    let mut probe = FakeProbe::new(vec![Ok(observation(1_000, 10.0))]);
    probe.called = Some(called_tx);
    let (copilot_called_tx, copilot_called_rx) = mpsc::channel();
    let mut copilot_probe = FakeCopilotProbe::new(vec![Ok(copilot_observation(1_000, 10.0))]);
    copilot_probe.called = Some(copilot_called_tx);
    let poller = DashboardPoller::start_with(
        storage,
        Some(Arc::new(probe)),
        Some(Arc::new(copilot_probe)),
        AppConfig {
            codex_refresh_seconds: 900,
            copilot_refresh_seconds: 3_600,
            ..AppConfig::default()
        },
        FixedClock(1_000),
    )
    .unwrap();
    called_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    copilot_called_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    poller.stop_and_join().unwrap();
}

#[test]
fn copilot_failure_is_isolated_from_codex_and_claude_health() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    storage
        .record_provider_attempt(Provider::Claude, 500)
        .unwrap();
    storage
        .record_provider_success(Provider::Claude, 500)
        .unwrap();

    let copilot = FakeCopilotProbe::new(vec![Err(CopilotError::NotAuthenticated)]);
    poll_copilot_once(
        &storage,
        &copilot,
        &AppConfig::default(),
        &FixedClock(1_000),
    );
    let codex = FakeProbe::new(vec![Ok(observation(2_000, 33.0))]);
    poll_codex_once(&storage, &codex, &AppConfig::default(), &FixedClock(2_000));

    let copilot_state = storage
        .provider_state(Provider::GitHubCopilot)
        .unwrap()
        .unwrap();
    assert_eq!(copilot_state.consecutive_failures, 1);
    assert_eq!(
        copilot_state.last_error_class.as_deref(),
        Some("not_authenticated")
    );
    let codex_state = storage.provider_state(Provider::Codex).unwrap().unwrap();
    assert_eq!(codex_state.last_success_at, Some(2_000));
    assert_eq!(codex_state.consecutive_failures, 0);
    let claude_state = storage.provider_state(Provider::Claude).unwrap().unwrap();
    assert_eq!(claude_state.last_success_at, Some(500));
    assert_eq!(claude_state.consecutive_failures, 0);
}

#[test]
fn dashboard_poller_supports_every_selective_disable_combination() {
    let temp = tempdir().unwrap();
    let config = AppConfig {
        codex_refresh_seconds: 900,
        copilot_refresh_seconds: 3_600,
        ..AppConfig::default()
    };

    let storage = Storage::open(temp.path().join("both.sqlite3")).unwrap();
    let (codex_tx, codex_rx) = mpsc::channel();
    let mut codex = FakeProbe::new(vec![]);
    codex.called = Some(codex_tx);
    let (copilot_tx, copilot_rx) = mpsc::channel();
    let mut copilot = FakeCopilotProbe::new(vec![]);
    copilot.called = Some(copilot_tx);
    let poller = DashboardPoller::start_with(
        storage,
        Some(Arc::new(codex)),
        Some(Arc::new(copilot)),
        config.clone(),
        FixedClock(1_000),
    )
    .unwrap();
    codex_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    copilot_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    poller.stop_and_join().unwrap();

    let storage = Storage::open(temp.path().join("codex-only.sqlite3")).unwrap();
    let (codex_tx, codex_rx) = mpsc::channel();
    let mut codex = FakeProbe::new(vec![]);
    codex.called = Some(codex_tx);
    let codex = Arc::new(codex);
    let poller = DashboardPoller::start_with(
        storage.clone(),
        Some(codex.clone()),
        None::<Arc<FakeCopilotProbe>>,
        config.clone(),
        FixedClock(1_000),
    )
    .unwrap();
    codex_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    poller.stop_and_join().unwrap();
    assert_eq!(codex.calls.load(Ordering::SeqCst), 1);
    assert!(
        storage
            .provider_state(Provider::GitHubCopilot)
            .unwrap()
            .is_none()
    );

    let storage = Storage::open(temp.path().join("copilot-only.sqlite3")).unwrap();
    let (copilot_tx, copilot_rx) = mpsc::channel();
    let mut copilot = FakeCopilotProbe::new(vec![]);
    copilot.called = Some(copilot_tx);
    let copilot = Arc::new(copilot);
    let poller = DashboardPoller::start_with(
        storage.clone(),
        None::<Arc<FakeProbe>>,
        Some(copilot.clone()),
        config.clone(),
        FixedClock(1_000),
    )
    .unwrap();
    copilot_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    poller.stop_and_join().unwrap();
    assert_eq!(copilot.calls.load(Ordering::SeqCst), 1);
    assert!(storage.provider_state(Provider::Codex).unwrap().is_none());

    let storage = Storage::open(temp.path().join("neither.sqlite3")).unwrap();
    DashboardPoller::start_with(
        storage,
        None::<Arc<FakeProbe>>,
        None::<Arc<FakeCopilotProbe>>,
        config,
        FixedClock(1_000),
    )
    .unwrap()
    .stop_and_join()
    .unwrap();
}

struct BlockingCopilotProbe {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    active_runtime: Arc<AtomicUsize>,
}

impl CopilotProbe for BlockingCopilotProbe {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.active_runtime.fetch_add(1, Ordering::SeqCst);
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        self.active_runtime.fetch_sub(1, Ordering::SeqCst);
        Ok(copilot_observation(observed_at, 15.0))
    }
}

#[test]
fn stop_and_join_waits_for_in_flight_runtime_and_leaves_no_orphan() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let active_runtime = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BlockingCopilotProbe {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        active_runtime: active_runtime.clone(),
    });
    let poller = DashboardPoller::start_with(
        storage,
        None::<Arc<FakeProbe>>,
        Some(probe),
        AppConfig::default(),
        FixedClock(1_000),
    )
    .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(active_runtime.load(Ordering::SeqCst), 1);

    let (joined_tx, joined_rx) = mpsc::channel();
    let joiner = std::thread::spawn(move || {
        let result = poller.stop_and_join();
        joined_tx.send(result).unwrap();
    });
    assert!(joined_rx.recv_timeout(Duration::from_millis(50)).is_err());
    release_tx.send(()).unwrap();
    joined_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    joiner.join().unwrap();
    assert_eq!(active_runtime.load(Ordering::SeqCst), 0);
}

struct DropTrackedCodexProbe {
    called: mpsc::Sender<()>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl CodexProbe for DropTrackedCodexProbe {
    fn read(&self, observed_at: i64) -> Result<CodexObservation, CodexError> {
        self.called.send(()).unwrap();
        Ok(observation(observed_at, 5.0))
    }
}

impl Drop for DropTrackedCodexProbe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct PanickingCopilotProbe {
    called: mpsc::Sender<()>,
}

impl CopilotProbe for PanickingCopilotProbe {
    fn read(&self, _observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.called.send(()).unwrap();
        panic!("injected worker failure");
    }
}

#[test]
fn worker_failure_is_reported_after_all_workers_are_stopped_and_joined() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let (codex_tx, codex_rx) = mpsc::channel();
    let codex_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let codex = Arc::new(DropTrackedCodexProbe {
        called: codex_tx,
        dropped: codex_dropped.clone(),
    });
    let (copilot_tx, copilot_rx) = mpsc::channel();
    let poller = DashboardPoller::start_with(
        storage,
        Some(codex),
        Some(Arc::new(PanickingCopilotProbe { called: copilot_tx })),
        AppConfig {
            codex_refresh_seconds: 900,
            copilot_refresh_seconds: 3_600,
            ..AppConfig::default()
        },
        FixedClock(1_000),
    )
    .unwrap();
    codex_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    copilot_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let error = poller.stop_and_join().unwrap_err();
    assert!(matches!(error, DashboardPollError::Join("GitHub Copilot")));
    assert!(codex_dropped.load(Ordering::SeqCst));
}
