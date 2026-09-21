use std::{
    io,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use thiserror::Error;

use crate::{
    alerts::{AlertEngine, Clock, SystemClock},
    config::AppConfig,
    domain::{Provider, QuotaSnapshot, normalize_rows},
    notify::{DesktopNotifier, TerminalNotifier, deliver},
    providers::{
        codex::{CodexAdapter, CodexError, CodexObservation},
        github_copilot::{CopilotAdapter, CopilotError, CopilotObservation},
    },
    storage::Storage,
};

pub trait CodexProbe: Send + Sync + 'static {
    fn read(&self, observed_at: i64) -> Result<CodexObservation, CodexError>;
}

impl CodexProbe for CodexAdapter {
    fn read(&self, observed_at: i64) -> Result<CodexObservation, CodexError> {
        self.read_rate_limits(observed_at)
    }
}

pub trait CopilotProbe: Send + Sync + 'static {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError>;
}

impl CopilotProbe for CopilotAdapter {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.read_quota(observed_at)
    }
}

#[derive(Debug, Error)]
pub enum DashboardPollError {
    #[error("unable to initialize the GitHub Copilot dashboard poller: {class}")]
    CopilotInitialization { class: &'static str },
    #[error("unable to start the {0} dashboard polling worker")]
    Start(&'static str),
    #[error("the {0} dashboard polling worker did not stop cleanly")]
    Join(&'static str),
}

pub struct DashboardPoller {
    workers: Vec<PollWorker>,
}

impl DashboardPoller {
    pub fn start(
        storage: Storage,
        executable: PathBuf,
        copilot_runtime: Option<PathBuf>,
        config: AppConfig,
        codex_enabled: bool,
        copilot_enabled: bool,
    ) -> Result<Self, DashboardPollError> {
        let codex_probe = codex_enabled.then(|| Arc::new(CodexAdapter::new(executable)));
        let copilot_probe = if copilot_enabled {
            Some(Arc::new(CopilotAdapter::new(copilot_runtime).map_err(
                |error| DashboardPollError::CopilotInitialization {
                    class: error.class(),
                },
            )?))
        } else {
            None
        };
        Self::start_with(storage, codex_probe, copilot_probe, config, SystemClock)
    }

    fn start_with<CP, GP, C>(
        storage: Storage,
        codex_probe: Option<Arc<CP>>,
        copilot_probe: Option<Arc<GP>>,
        config: AppConfig,
        clock: C,
    ) -> Result<Self, DashboardPollError>
    where
        CP: CodexProbe,
        GP: CopilotProbe,
        C: Clock + Clone + Send + Sync + 'static,
    {
        let mut poller = Self {
            workers: Vec::with_capacity(2),
        };
        if let Some(probe) = codex_probe {
            let worker_storage = storage.clone();
            let worker_config = config.clone();
            let worker_clock = clock.clone();
            poller.workers.push(PollWorker::spawn("Codex", move |stop| {
                run_codex_poll_loop(
                    &worker_storage,
                    probe.as_ref(),
                    &worker_config,
                    &worker_clock,
                    stop.as_ref(),
                );
            })?);
        }
        if let Some(probe) = copilot_probe {
            let worker_storage = storage;
            let worker_config = config;
            poller
                .workers
                .push(PollWorker::spawn("GitHub Copilot", move |stop| {
                    run_copilot_poll_loop(
                        &worker_storage,
                        probe.as_ref(),
                        &worker_config,
                        &clock,
                        stop.as_ref(),
                    );
                })?);
        }
        Ok(poller)
    }

    pub fn stop_and_join(mut self) -> Result<(), DashboardPollError> {
        self.stop_all();
        self.join_all()
    }

    fn stop_all(&self) {
        for worker in &self.workers {
            worker.stop.stop();
        }
    }

    fn join_all(&mut self) -> Result<(), DashboardPollError> {
        let mut first_error = None;
        for worker in &mut self.workers {
            if let Err(error) = worker.join()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for DashboardPoller {
    fn drop(&mut self) {
        self.stop_all();
        let _ = self.join_all();
    }
}

struct PollWorker {
    provider: &'static str,
    stop: Arc<StopSignal>,
    handle: Option<JoinHandle<()>>,
}

impl PollWorker {
    fn spawn(
        provider: &'static str,
        run: impl FnOnce(Arc<StopSignal>) + Send + 'static,
    ) -> Result<Self, DashboardPollError> {
        let stop = Arc::new(StopSignal::default());
        let worker_stop = stop.clone();
        let handle = thread::Builder::new()
            .name(format!(
                "{}-dashboard-poll",
                provider.to_ascii_lowercase().replace(' ', "-")
            ))
            .spawn(move || run(worker_stop))
            .map_err(|_| DashboardPollError::Start(provider))?;
        Ok(Self {
            provider,
            stop,
            handle: Some(handle),
        })
    }

    fn join(&mut self) -> Result<(), DashboardPollError> {
        self.handle.take().map_or(Ok(()), |handle| {
            handle
                .join()
                .map_err(|_| DashboardPollError::Join(self.provider))
        })
    }
}

trait PollWait {
    /// Returns true when polling should stop.
    fn wait(&self, duration: Duration) -> bool;
    fn stopped(&self) -> bool;
}

#[derive(Default)]
struct StopSignal {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl StopSignal {
    fn stop(&self) {
        if let Ok(mut stopped) = self.stopped.lock() {
            *stopped = true;
            self.wake.notify_all();
        }
    }
}

impl PollWait for StopSignal {
    fn wait(&self, duration: Duration) -> bool {
        let Ok(stopped) = self.stopped.lock() else {
            return true;
        };
        if *stopped {
            return true;
        }
        self.wake
            .wait_timeout_while(stopped, duration, |value| !*value)
            .map_or(true, |(value, _)| *value)
    }

    fn stopped(&self) -> bool {
        self.stopped.lock().map_or(true, |value| *value)
    }
}

fn run_codex_poll_loop<P: CodexProbe>(
    storage: &Storage,
    probe: &P,
    config: &AppConfig,
    clock: &impl Clock,
    wait: &impl PollWait,
) {
    let interval = Duration::from_secs(config.codex_refresh_seconds);
    while !wait.stopped() {
        poll_codex_once(storage, probe, config, clock);
        if wait.wait(interval) {
            break;
        }
    }
}

fn run_copilot_poll_loop<P: CopilotProbe>(
    storage: &Storage,
    probe: &P,
    config: &AppConfig,
    clock: &impl Clock,
    wait: &impl PollWait,
) {
    let interval = Duration::from_secs(config.copilot_refresh_seconds);
    while !wait.stopped() {
        poll_copilot_once(storage, probe, config, clock);
        if wait.wait(interval) {
            break;
        }
    }
}

fn poll_codex_once(
    storage: &Storage,
    probe: &impl CodexProbe,
    config: &AppConfig,
    clock: &impl Clock,
) {
    let now = clock.now_millis();
    if storage
        .record_provider_attempt(Provider::Codex, now)
        .is_err()
    {
        return;
    }
    let observation = match probe.read(now) {
        Ok(observation) => observation,
        Err(error) => {
            let _ = storage.record_provider_failure(Provider::Codex, now, codex_error_class(error));
            return;
        }
    };
    let batch = normalize_rows(observation.snapshots);
    if persist_success(storage, &batch.accepted, config, clock).is_err() {
        let _ = storage.record_provider_failure(Provider::Codex, now, "storage");
        return;
    }
    let _ = storage.record_provider_success(Provider::Codex, now);
}

fn poll_copilot_once(
    storage: &Storage,
    probe: &impl CopilotProbe,
    config: &AppConfig,
    clock: &impl Clock,
) {
    let now = clock.now_millis();
    if storage
        .record_provider_attempt(Provider::GitHubCopilot, now)
        .is_err()
    {
        return;
    }
    let observation = match probe.read(now) {
        Ok(observation) => observation,
        Err(error) => {
            let _ = storage.record_provider_failure(Provider::GitHubCopilot, now, error.class());
            return;
        }
    };
    let batch = normalize_rows(observation.snapshots);
    if persist_success(storage, &batch.accepted, config, clock).is_err() {
        let _ = storage.record_provider_failure(Provider::GitHubCopilot, now, "storage");
        return;
    }
    let _ = storage.record_provider_success(Provider::GitHubCopilot, now);
}

fn persist_success(
    storage: &Storage,
    snapshots: &[QuotaSnapshot],
    config: &AppConfig,
    clock: &impl Clock,
) -> Result<(), crate::storage::StorageError> {
    storage.insert_snapshots(snapshots)?;
    let events = AlertEngine::new(config.clone()).process(storage, snapshots, clock)?;
    let mut hidden_terminal = TerminalNotifier::new(io::sink(), false);
    let mut desktop = DesktopNotifier;
    let native = config
        .native_notifications
        .then_some(&mut desktop as &mut dyn crate::notify::Notifier);
    let _ = deliver(&events, &mut hidden_terminal, native);
    let now = clock.now_millis();
    let retention_millis = i64::from(config.retention_days).saturating_mul(86_400_000);
    let cutoff = now.saturating_sub(retention_millis).max(0);
    storage.prune_before(cutoff)?;
    storage.prune_alerts_before(cutoff)?;
    Ok(())
}

fn codex_error_class(error: CodexError) -> &'static str {
    match error {
        CodexError::Spawn => "spawn",
        CodexError::Transport { .. } => "transport",
        CodexError::Timeout { .. } => "timeout",
        CodexError::Protocol { .. } => "protocol",
        CodexError::Rpc { .. } => "rpc",
    }
}

#[cfg(test)]
#[path = "dashboard_polling_tests.rs"]
mod dashboard_polling_tests;
