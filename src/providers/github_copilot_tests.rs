use std::{
    collections::BTreeMap,
    future::pending,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::domain::{Availability, LimitKind, Quality, WindowKind};

use super::*;

const OBSERVED_AT: i64 = 1_780_000_000_123;
const SHORT_TIMEOUT: Duration = Duration::from_millis(2);

fn quota(entitlement_requests: i64, remaining_percentage: f64) -> QuotaValue {
    QuotaValue {
        entitlement_requests,
        remaining_percentage,
        reset_date: Some("2026-09-30T12:34:56Z".into()),
    }
}

fn row<'a>(observation: &'a CopilotObservation, scope_key: &str) -> &'a QuotaSnapshotInput {
    observation
        .snapshots
        .iter()
        .find(|row| row.scope_key == scope_key)
        .unwrap()
}

#[test]
fn normalizes_common_unknown_and_percentage_boundaries() {
    let quotas = BTreeMap::from([
        ("premium_interactions".into(), quota(300, 0.0)),
        ("future_runtime_bucket".into(), quota(50, 100.0)),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    assert_eq!(observation.rejected_windows, 0);
    assert_eq!(observation.snapshots.len(), 2);
    let exhausted = row(&observation, "premium_interactions");
    assert_eq!(exhausted.used_percent, 100.0);
    assert_eq!(exhausted.availability, Availability::Blocked);
    assert_eq!(exhausted.window_kind, WindowKind::Monthly);
    assert_eq!(exhausted.limit_kind, LimitKind::Limited);
    let unused = row(&observation, "future_runtime_bucket");
    assert_eq!(unused.used_percent, 0.0);
    assert_eq!(unused.availability, Availability::Allowed);
}

#[test]
fn unlimited_and_invalid_entitlement_are_handled_per_row() {
    let quotas = BTreeMap::from([
        ("chat".into(), quota(-1, 0.0)),
        ("invalid".into(), quota(-2, 50.0)),
        ("completions".into(), quota(100, 75.0)),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    assert_eq!(observation.snapshots.len(), 2);
    assert_eq!(observation.rejected_windows, 1);
    let unlimited = row(&observation, "chat");
    assert_eq!(unlimited.limit_kind, LimitKind::Unlimited);
    assert_eq!(unlimited.used_percent, 0.0);
    assert_eq!(unlimited.availability, Availability::Allowed);
    assert_eq!(row(&observation, "completions").used_percent, 25.0);
}

#[test]
fn invalid_finite_percentages_reject_only_their_rows() {
    let quotas = BTreeMap::from([
        ("below_zero".into(), quota(100, -0.1)),
        ("above_hundred".into(), quota(100, 100.1)),
        ("not_finite".into(), quota(100, f64::NAN)),
        ("valid_sibling".into(), quota(100, 40.0)),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    assert_eq!(observation.snapshots.len(), 1);
    assert_eq!(observation.rejected_windows, 3);
    assert_eq!(observation.snapshots[0].scope_key, "valid_sibling");
    assert_eq!(observation.snapshots[0].used_percent, 60.0);
}

#[test]
fn reset_dates_are_parsed_or_rejected_without_losing_siblings() {
    let mut missing = quota(100, 70.0);
    missing.reset_date = None;
    let mut with_offset = quota(100, 60.0);
    with_offset.reset_date = Some("2026-10-01T14:00:00+02:00".into());
    let mut invalid = quota(100, 50.0);
    invalid.reset_date = Some("not-a-date".into());
    let quotas = BTreeMap::from([
        ("missing_reset".into(), missing),
        ("valid_reset".into(), with_offset),
        ("invalid_reset".into(), invalid),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    assert_eq!(observation.snapshots.len(), 2);
    assert_eq!(observation.rejected_windows, 1);
    let missing = row(&observation, "missing_reset");
    assert_eq!(missing.resets_at, None);
    assert_eq!(missing.quality, Quality::Partial);
    let valid = row(&observation, "valid_reset");
    assert_eq!(valid.resets_at, Some(1_790_856_000));
    assert_eq!(valid.quality, Quality::Fresh);
}

#[test]
fn poll_time_reset_dates_are_treated_as_missing() {
    let observed_at = 1_790_856_000_123;
    let quotas = BTreeMap::from([
        (
            "premium_interactions".into(),
            QuotaValue {
                entitlement_requests: 300,
                remaining_percentage: 75.0,
                reset_date: Some("2026-10-01T14:00:00+02:00".into()),
            },
        ),
        (
            "runtime_fetch_lag".into(),
            QuotaValue {
                entitlement_requests: 100,
                remaining_percentage: 50.0,
                reset_date: Some("2026-10-01T12:00:20Z".into()),
            },
        ),
    ]);

    let observation = normalize_quota_map(quotas, observed_at);

    assert_eq!(observation.rejected_windows, 0);
    for scope_key in ["premium_interactions", "runtime_fetch_lag"] {
        let quota = row(&observation, scope_key);
        assert_eq!(quota.resets_at, None);
        assert_eq!(quota.quality, Quality::Partial);
    }
}

#[test]
fn unsafe_and_oversized_keys_reject_only_their_rows() {
    let quotas = BTreeMap::from([
        ("safe_unknown".into(), quota(1, 50.0)),
        ("unsafe\nkey".into(), quota(1, 50.0)),
        ("x".repeat(129), quota(1, 50.0)),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    assert_eq!(observation.snapshots.len(), 1);
    assert_eq!(observation.rejected_windows, 2);
    assert_eq!(observation.snapshots[0].scope_key, "safe_unknown");
}

#[test]
fn empty_and_duplicate_maps_cannot_fabricate_rows() {
    let empty = normalize_quota_map(BTreeMap::new(), OBSERVED_AT);
    assert!(empty.snapshots.is_empty());
    assert_eq!(empty.rejected_windows, 0);

    let mut quotas = BTreeMap::new();
    quotas.insert("chat".into(), quota(100, 80.0));
    quotas.insert("chat".into(), quota(100, 70.0));
    let observation = normalize_quota_map(quotas, OBSERVED_AT);
    assert_eq!(observation.snapshots.len(), 1);
    assert_eq!(observation.snapshots[0].used_percent, 30.0);
}

#[test]
fn all_rows_share_one_observation_time_and_sequence() {
    let quotas = BTreeMap::from([
        ("chat".into(), quota(100, 80.0)),
        ("completions".into(), quota(100, 70.0)),
        ("other".into(), quota(100, 60.0)),
    ]);

    let observation = normalize_quota_map(quotas, OBSERVED_AT);

    let sequence = observation.snapshots[0].source_sequence;
    assert!(
        observation
            .snapshots
            .iter()
            .all(|row| row.observed_at == OBSERVED_AT && row.source_sequence == sequence)
    );
}

#[derive(Clone)]
enum FakeBehavior<T> {
    Ok(T),
    Err(CopilotError),
    Pending,
}

impl<T: Clone + Send + 'static> FakeBehavior<T> {
    async fn resolve(&self) -> Result<T, CopilotError> {
        match self {
            Self::Ok(value) => Ok(value.clone()),
            Self::Err(error) => Err(*error),
            Self::Pending => pending().await,
        }
    }
}

#[derive(Default)]
struct FakeTrace {
    events: Mutex<Vec<&'static str>>,
    programs: Mutex<Vec<CopilotProgram>>,
}

impl FakeTrace {
    fn event(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

struct FakeFactory {
    trace: Arc<FakeTrace>,
    start: FakeBehavior<()>,
    rpc: FakeBehavior<QuotaMap>,
    stop: FakeBehavior<()>,
}

impl QuotaClientFactory for FakeFactory {
    fn start(
        &self,
        program: CopilotProgram,
    ) -> ClientFuture<'_, Result<Box<dyn QuotaClient>, CopilotError>> {
        Box::pin(async move {
            self.trace.event("start");
            self.trace.programs.lock().unwrap().push(program);
            self.start.resolve().await?;
            Ok(Box::new(FakeClient {
                trace: Arc::clone(&self.trace),
                rpc: self.rpc.clone(),
                stop: self.stop.clone(),
            }) as Box<dyn QuotaClient>)
        })
    }
}

struct FakeClient {
    trace: Arc<FakeTrace>,
    rpc: FakeBehavior<QuotaMap>,
    stop: FakeBehavior<()>,
}

impl QuotaClient for FakeClient {
    fn get_quota(&self) -> ClientFuture<'_, Result<QuotaMap, CopilotError>> {
        Box::pin(async move {
            self.trace.event("rpc");
            self.rpc.resolve().await
        })
    }

    fn stop(&self) -> ClientFuture<'_, Result<(), CopilotError>> {
        Box::pin(async move {
            self.trace.event("stop");
            self.stop.resolve().await
        })
    }

    fn force_stop(&self) {
        self.trace.event("force_stop");
    }
}

fn adapter(
    program: CopilotProgram,
    start: FakeBehavior<()>,
    rpc: FakeBehavior<QuotaMap>,
    stop: FakeBehavior<()>,
) -> (CopilotAdapter, Arc<FakeTrace>) {
    let trace = Arc::new(FakeTrace::default());
    let factory = Arc::new(FakeFactory {
        trace: Arc::clone(&trace),
        start,
        rpc,
        stop,
    });
    let adapter = CopilotAdapter::with_factory(
        program,
        factory,
        CopilotTimeouts {
            start: SHORT_TIMEOUT,
            rpc: SHORT_TIMEOUT,
            shutdown: SHORT_TIMEOUT,
        },
    )
    .unwrap();
    (adapter, trace)
}

fn one_quota() -> QuotaMap {
    BTreeMap::from([("chat".into(), quota(100, 50.0))])
}

#[test]
fn lifecycle_is_one_start_rpc_and_stop_with_no_session_surface() {
    let runtime_path = PathBuf::from("C:/explicit/copilot-runtime.exe");
    let (adapter, trace) = adapter(
        CopilotProgram::Path(runtime_path.clone()),
        FakeBehavior::Ok(()),
        FakeBehavior::Ok(one_quota()),
        FakeBehavior::Ok(()),
    );

    let observation = adapter.read_quota(OBSERVED_AT).unwrap();

    assert_eq!(observation.snapshots.len(), 1);
    assert_eq!(trace.events(), ["start", "rpc", "stop"]);
    assert_eq!(
        *trace.programs.lock().unwrap(),
        [CopilotProgram::Path(runtime_path)]
    );
    assert!(trace.events().iter().all(|event| *event != "session"));
}

#[test]
fn rpc_failure_still_attempts_stop_and_preserves_primary_error() {
    let (adapter, trace) = adapter(
        CopilotProgram::Bundled,
        FakeBehavior::Ok(()),
        FakeBehavior::Err(CopilotError::NotAuthenticated),
        FakeBehavior::Err(CopilotError::Rpc),
    );

    let error = adapter.read_quota(OBSERVED_AT).unwrap_err();

    assert_eq!(error, CopilotError::NotAuthenticated);
    assert_eq!(trace.events(), ["start", "rpc", "stop", "force_stop"]);
}

#[test]
fn startup_timeout_is_bounded() {
    let (adapter, trace) = adapter(
        CopilotProgram::Bundled,
        FakeBehavior::Pending,
        FakeBehavior::Ok(one_quota()),
        FakeBehavior::Ok(()),
    );

    let error = adapter.read_quota(OBSERVED_AT).unwrap_err();

    assert_eq!(error, CopilotError::Timeout(CopilotPhase::Start));
    assert_eq!(trace.events(), ["start"]);
}

#[test]
fn rpc_timeout_is_bounded_and_still_stops() {
    let (adapter, trace) = adapter(
        CopilotProgram::Bundled,
        FakeBehavior::Ok(()),
        FakeBehavior::Pending,
        FakeBehavior::Ok(()),
    );

    let error = adapter.read_quota(OBSERVED_AT).unwrap_err();

    assert_eq!(error, CopilotError::Timeout(CopilotPhase::Rpc));
    assert_eq!(trace.events(), ["start", "rpc", "stop"]);
}

#[test]
fn shutdown_timeout_is_bounded_and_forces_stop() {
    let (adapter, trace) = adapter(
        CopilotProgram::Bundled,
        FakeBehavior::Ok(()),
        FakeBehavior::Ok(one_quota()),
        FakeBehavior::Pending,
    );

    let error = adapter.read_quota(OBSERVED_AT).unwrap_err();

    assert_eq!(error, CopilotError::Timeout(CopilotPhase::Shutdown));
    assert_eq!(trace.events(), ["start", "rpc", "stop", "force_stop"]);
}

#[test]
fn exposed_errors_are_stable_classes_without_provider_text() {
    let errors = [
        CopilotError::Spawn,
        CopilotError::Timeout(CopilotPhase::Rpc),
        CopilotError::NotAuthenticated,
        CopilotError::NotEntitled,
        CopilotError::Protocol,
        CopilotError::Rpc,
    ];
    let classes: Vec<_> = errors.iter().map(|error| error.class()).collect();

    assert_eq!(
        classes,
        [
            "spawn",
            "timeout",
            "not_authenticated",
            "not_entitled",
            "protocol",
            "rpc"
        ]
    );
    assert!(
        errors
            .iter()
            .all(|error| !error.to_string().contains("raw-detail-marker"))
    );
}
