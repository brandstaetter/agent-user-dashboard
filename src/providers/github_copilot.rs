use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration,
};

use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, Error as SdkError, ErrorKind as SdkErrorKind, Transport,
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::runtime::{Builder, Runtime};
use uuid::Uuid;

use crate::domain::{
    Availability, LimitKind, Provider, Quality, QuotaSnapshot, QuotaSnapshotInput, WindowKind,
};

const DEFAULT_PHASE_TIMEOUT: Duration = Duration::from_secs(10);
// Copilot CLI 1.0.83 can return the quota fetch time in `resetDate`. The
// observation timestamp is captured before startup/RPC, whose combined timeout
// is 20 seconds, so values in this window cannot be trusted as monthly resets.
const RESET_FETCH_TIMESTAMP_TOLERANCE_SECONDS: i64 = 30;

type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq)]
pub struct CopilotObservation {
    pub snapshots: Vec<QuotaSnapshotInput>,
    pub rejected_windows: usize,
}

impl CopilotObservation {
    pub fn quality(&self) -> Quality {
        if self.rejected_windows == 0
            && !self.snapshots.is_empty()
            && self
                .snapshots
                .iter()
                .all(|row| row.quality == Quality::Fresh)
        {
            Quality::Fresh
        } else {
            Quality::Partial
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotPhase {
    Start,
    Rpc,
    Shutdown,
}

impl CopilotPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Rpc => "quota",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CopilotError {
    #[error("unable to start the GitHub Copilot runtime")]
    Spawn,
    #[error("GitHub Copilot {phase} timed out", phase = .0.as_str())]
    Timeout(CopilotPhase),
    #[error("GitHub Copilot is not authenticated")]
    NotAuthenticated,
    #[error("GitHub Copilot quota is not available for this entitlement")]
    NotEntitled,
    #[error("GitHub Copilot violated the quota protocol")]
    Protocol,
    #[error("GitHub Copilot quota request failed")]
    Rpc,
}

impl CopilotError {
    pub const fn class(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Timeout(_) => "timeout",
            Self::NotAuthenticated => "not_authenticated",
            Self::NotEntitled => "not_entitled",
            Self::Protocol => "protocol",
            Self::Rpc => "rpc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotProgram {
    Bundled,
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CopilotTimeouts {
    pub start: Duration,
    pub rpc: Duration,
    pub shutdown: Duration,
}

impl Default for CopilotTimeouts {
    fn default() -> Self {
        Self {
            start: DEFAULT_PHASE_TIMEOUT,
            rpc: DEFAULT_PHASE_TIMEOUT,
            shutdown: DEFAULT_PHASE_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaValue {
    pub entitlement_requests: i64,
    pub remaining_percentage: f64,
    pub reset_date: Option<String>,
}

pub(crate) type QuotaMap = BTreeMap<String, QuotaValue>;

pub(crate) trait QuotaClient: Send + Sync {
    fn get_quota(&self) -> ClientFuture<'_, Result<QuotaMap, CopilotError>>;
    fn stop(&self) -> ClientFuture<'_, Result<(), CopilotError>>;
    fn force_stop(&self);
}

pub(crate) trait QuotaClientFactory: Send + Sync {
    fn start(
        &self,
        program: CopilotProgram,
    ) -> ClientFuture<'_, Result<Box<dyn QuotaClient>, CopilotError>>;
}

#[derive(Debug, Default)]
struct SdkQuotaClientFactory;

impl QuotaClientFactory for SdkQuotaClientFactory {
    fn start(
        &self,
        program: CopilotProgram,
    ) -> ClientFuture<'_, Result<Box<dyn QuotaClient>, CopilotError>> {
        Box::pin(async move {
            let mut options = ClientOptions::new().with_transport(Transport::Stdio);
            if let CopilotProgram::Path(path) = program {
                options = options.with_program(CliProgram::Path(path));
            }
            Client::start(options)
                .await
                .map(|client| Box::new(SdkQuotaClient { client }) as Box<dyn QuotaClient>)
                .map_err(map_start_error)
        })
    }
}

struct SdkQuotaClient {
    client: Client,
}

impl QuotaClient for SdkQuotaClient {
    fn get_quota(&self) -> ClientFuture<'_, Result<QuotaMap, CopilotError>> {
        Box::pin(async move {
            let result = self
                .client
                .rpc()
                .account()
                .get_quota()
                .await
                .map_err(map_rpc_error)?;
            Ok(result
                .quota_snapshots
                .into_iter()
                .map(|(key, value)| {
                    (
                        key,
                        QuotaValue {
                            entitlement_requests: value.entitlement_requests,
                            remaining_percentage: value.remaining_percentage,
                            reset_date: value.reset_date,
                        },
                    )
                })
                .collect())
        })
    }

    fn stop(&self) -> ClientFuture<'_, Result<(), CopilotError>> {
        Box::pin(async move { self.client.stop().await.map_err(|_| CopilotError::Rpc) })
    }

    fn force_stop(&self) {
        self.client.force_stop();
    }
}

pub struct CopilotAdapter {
    runtime: Runtime,
    factory: Arc<dyn QuotaClientFactory>,
    program: CopilotProgram,
    timeouts: CopilotTimeouts,
}

impl CopilotAdapter {
    pub fn new(runtime_path: Option<PathBuf>) -> Result<Self, CopilotError> {
        let program = runtime_path.map_or(CopilotProgram::Bundled, CopilotProgram::Path);
        Self::with_factory(
            program,
            Arc::new(SdkQuotaClientFactory),
            CopilotTimeouts::default(),
        )
    }

    pub(crate) fn with_factory(
        program: CopilotProgram,
        factory: Arc<dyn QuotaClientFactory>,
        timeouts: CopilotTimeouts,
    ) -> Result<Self, CopilotError> {
        let runtime = Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|_| CopilotError::Spawn)?;
        Ok(Self {
            runtime,
            factory,
            program,
            timeouts,
        })
    }

    pub fn read_quota(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.runtime.block_on(self.read_quota_async(observed_at))
    }

    async fn read_quota_async(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        let client = tokio::time::timeout(
            self.timeouts.start,
            self.factory.start(self.program.clone()),
        )
        .await
        .map_err(|_| CopilotError::Timeout(CopilotPhase::Start))??;

        let quota_result = tokio::time::timeout(self.timeouts.rpc, client.get_quota())
            .await
            .map_err(|_| CopilotError::Timeout(CopilotPhase::Rpc))
            .and_then(|result| result);

        let stop_result = match tokio::time::timeout(self.timeouts.shutdown, client.stop()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                client.force_stop();
                Err(error)
            }
            Err(_) => {
                client.force_stop();
                Err(CopilotError::Timeout(CopilotPhase::Shutdown))
            }
        };

        let quotas = quota_result?;
        stop_result?;
        Ok(normalize_quota_map(quotas, observed_at))
    }
}

pub(crate) fn normalize_quota_map(quotas: QuotaMap, observed_at: i64) -> CopilotObservation {
    let source_sequence = Uuid::new_v4();
    let mut observation = CopilotObservation {
        snapshots: Vec::with_capacity(quotas.len()),
        rejected_windows: 0,
    };

    for (scope_key, value) in quotas {
        match normalize_quota(scope_key, value, observed_at, source_sequence) {
            Some(row) => observation.snapshots.push(row),
            None => observation.rejected_windows += 1,
        }
    }
    observation
}

fn normalize_quota(
    scope_key: String,
    value: QuotaValue,
    observed_at: i64,
    source_sequence: Uuid,
) -> Option<QuotaSnapshotInput> {
    if value.entitlement_requests < -1 {
        return None;
    }

    let limit_kind = if value.entitlement_requests == -1 {
        LimitKind::Unlimited
    } else {
        LimitKind::Limited
    };
    let used_percent = if limit_kind == LimitKind::Unlimited {
        0.0
    } else {
        100.0 - value.remaining_percentage
    };
    let resets_at = match value.reset_date {
        None => None,
        Some(reset_date) => {
            let parsed = OffsetDateTime::parse(&reset_date, &Rfc3339)
                .ok()?
                .unix_timestamp();
            let earliest_meaningful_reset =
                (observed_at / 1_000).saturating_add(RESET_FETCH_TIMESTAMP_TOLERANCE_SECONDS);
            (parsed > earliest_meaningful_reset).then_some(parsed)
        }
    };
    let quality = if resets_at.is_some() {
        Quality::Fresh
    } else {
        Quality::Partial
    };
    let availability = if limit_kind == LimitKind::Unlimited || value.remaining_percentage > 0.0 {
        Availability::Allowed
    } else {
        Availability::Blocked
    };
    let input = QuotaSnapshotInput {
        provider: Provider::GitHubCopilot,
        scope_key,
        window_kind: WindowKind::Monthly,
        limit_kind,
        window_duration_seconds: None,
        used_percent,
        resets_at,
        observed_at,
        availability,
        source_version: Some("github-copilot-sdk/1.0.13".into()),
        quality,
        source_sequence,
    };
    QuotaSnapshot::new(input.clone()).ok().map(|_| input)
}

fn map_start_error(error: SdkError) -> CopilotError {
    match error.kind() {
        SdkErrorKind::Io | SdkErrorKind::BinaryNotFound { .. } | SdkErrorKind::InvalidConfig => {
            CopilotError::Spawn
        }
        SdkErrorKind::Protocol(_) | SdkErrorKind::Json => CopilotError::Protocol,
        SdkErrorKind::Rpc { .. } | SdkErrorKind::Session(_) | SdkErrorKind::GitHubTokenProvider => {
            CopilotError::Rpc
        }
        _ => CopilotError::Protocol,
    }
}

fn map_rpc_error(error: SdkError) -> CopilotError {
    match error.kind() {
        SdkErrorKind::Protocol(_) | SdkErrorKind::Json => CopilotError::Protocol,
        _ => CopilotError::Rpc,
    }
}

#[cfg(test)]
#[path = "github_copilot_tests.rs"]
mod tests;
