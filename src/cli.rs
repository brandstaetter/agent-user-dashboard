use std::{
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use thiserror::Error;

use crate::{
    alerts::{AlertEngine, Clock},
    config::{AppConfig, AppPaths, ConfigError, ConfigFileError, PathError},
    dashboard_poll::{CodexProbe, DashboardPollError, DashboardPoller},
    domain::{Provider, QuotaSnapshot, QuotaSnapshotInput, ValidationError, normalize_rows},
    notify::{DesktopNotifier, TerminalNotifier, deliver},
    providers::{
        antigravity::{
            AntigravityParseError, MAX_STATUS_INPUT_BYTES as ANTIGRAVITY_MAX_STATUS_INPUT_BYTES,
            parse_status_line as parse_antigravity_status_line,
        },
        claude::{
            ClaudeParseError, MAX_STATUS_INPUT_BYTES as CLAUDE_MAX_STATUS_INPUT_BYTES,
            parse_status_line as parse_claude_status_line,
        },
        codex::{CodexAdapter, CodexError},
        github_copilot::{CopilotAdapter, CopilotError, CopilotObservation},
    },
    status_line_relay::{self, CapturedInput},
    storage::{Storage, StorageError},
    tui,
};

#[path = "cli_history.rs"]
mod history;
#[path = "cli_watch.rs"]
mod watch;

#[derive(Debug, Parser)]
#[command(name = "agent-usage-dashboard", version, about)]
pub struct Cli {
    /// Store the SQLite database in this directory instead of the OS data directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Read or write settings at this path instead of the OS config directory.
    #[arg(long, global = true, value_name = "FILE")]
    pub config_file: Option<PathBuf>,

    /// Codex polling interval, validated to 30-900 seconds.
    #[arg(long, global = true)]
    pub codex_refresh_seconds: Option<u64>,
    /// GitHub Copilot polling interval, validated to 60-3600 seconds.
    #[arg(long, global = true)]
    pub copilot_refresh_seconds: Option<u64>,
    /// Claude observations become stale after this many seconds.
    #[arg(long, global = true)]
    pub claude_stale_after_seconds: Option<u64>,
    /// Google Antigravity observations become stale after this many seconds.
    #[arg(long, global = true)]
    pub antigravity_stale_after_seconds: Option<u64>,
    /// Local snapshot and alert retention in days.
    #[arg(long, global = true)]
    pub retention_days: Option<u32>,
    /// Alert after remaining quota crosses this percentage.
    #[arg(long, global = true)]
    pub low_remaining_percent: Option<f64>,
    /// Alert when a known reset is this many seconds away.
    #[arg(long, global = true)]
    pub reset_soon_seconds: Option<u64>,
    /// Also attempt best-effort native desktop notifications.
    #[arg(long, global = true, action = clap::ArgAction::SetTrue, conflicts_with = "no_native_notifications")]
    pub native_notifications: bool,
    /// Disable native desktop notifications when persisted settings enable them.
    #[arg(long, global = true, action = clap::ArgAction::SetTrue)]
    pub no_native_notifications: bool,

    /// Codex executable used by the unified dashboard poller.
    #[arg(long, global = true, default_value = "codex", value_name = "PATH")]
    pub codex_executable: PathBuf,

    /// Disable dashboard Codex polling for offline operation.
    #[arg(long, global = true)]
    pub no_codex_poll: bool,

    /// Exact GitHub Copilot runtime program path; the bundled runtime is used when omitted.
    #[arg(long, global = true, value_name = "PATH")]
    pub copilot_runtime: Option<PathBuf>,

    /// Disable dashboard GitHub Copilot polling for offline operation.
    #[arg(long, global = true)]
    pub no_copilot_poll: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the unified terminal dashboard (the default command).
    Dashboard,
    /// Accept a provider observation from standard input.
    Ingest {
        #[command(subcommand)]
        provider: IngestCommand,
    },
    /// Relay provider JSON through an existing status-line formatter.
    Relay {
        #[command(subcommand)]
        provider: RelayCommand,
    },
    /// Perform one read-only provider refresh and exit.
    Refresh {
        #[command(subcommand)]
        provider: RefreshCommand,
    },
    /// Continuously perform bounded provider refreshes until interrupted.
    Watch {
        #[command(subcommand)]
        provider: WatchCommand,
    },
    /// Print bounded normalized snapshot and alert history.
    History {
        #[arg(long, default_value_t = 50, value_parser = parse_history_limit)]
        limit: usize,
    },
    /// Inspect or explicitly persist local dashboard settings.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show the effective settings after explicit CLI overrides.
    Show,
    /// Save the effective settings to the local config file.
    Save,
}

#[derive(Debug, Subcommand)]
pub enum IngestCommand {
    /// Read exactly one Claude Code status-line JSON object from stdin.
    Claude,
    /// Read exactly one Google Antigravity status-line JSON object from stdin.
    Antigravity,
}

#[derive(Debug, Subcommand)]
pub enum RelayCommand {
    /// Persist normalized quota fields while preserving a wrapped command exactly.
    Claude {
        /// Executable and arguments supplied after `--`; no shell is used.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Persist normalized Antigravity quota fields while preserving a wrapped command exactly.
    Antigravity {
        /// Executable and arguments supplied after `--`; no shell is used.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RefreshCommand {
    /// Query Codex app-server once and persist its normalized rate limits.
    Codex {
        /// Codex executable name or path.
        #[arg(long, default_value = "codex", value_name = "PATH")]
        executable: PathBuf,
    },
    /// Query GitHub Copilot account quota once and persist normalized windows.
    Copilot,
}

#[derive(Debug, Subcommand)]
pub enum WatchCommand {
    /// Continuously poll Codex app-server at the configured bounded interval.
    Codex {
        /// Codex executable name or path.
        #[arg(long, default_value = "codex", value_name = "PATH")]
        executable: PathBuf,
    },
    /// Continuously poll GitHub Copilot account quota at the configured bounded interval.
    Copilot,
}

trait CopilotProbe {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError>;
}

impl CopilotProbe for CopilotAdapter {
    fn read(&self, observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.read_quota(observed_at)
    }
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Paths(#[from] PathError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    ConfigFile(#[from] ConfigFileError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("standard input/output failed")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Claude(#[from] ClaudeParseError),
    #[error("Google Antigravity ingestion failed: {class}")]
    AntigravityIngest { class: &'static str },
    #[error(transparent)]
    Codex(#[from] CodexError),
    #[error("GitHub Copilot refresh failed: {class}")]
    CopilotRefresh { class: &'static str },
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Dashboard(#[from] tui::TuiError),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("unable to install the shutdown handler")]
    ShutdownHandler(#[from] ctrlc::Error),
    #[error(transparent)]
    DashboardPoll(#[from] DashboardPollError),
    #[error(transparent)]
    Relay(#[from] status_line_relay::RelayError),
    #[error("wrapped status-line command exited with status {0}")]
    WrappedExit(i32),
}

impl CliError {
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::WrappedExit(code) => Some(*code),
            _ => None,
        }
    }
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    if let Some(Command::Relay { provider }) = &cli.command {
        let context = relay_context(&cli);
        let observed_at = now_millis().unwrap_or(0);
        let outcome = match provider {
            RelayCommand::Claude { command } => {
                let (program, args) = command.split_first().expect("clap requires a command");
                status_line_relay::relay(
                    io::stdin().lock(),
                    io::stdout().lock(),
                    io::stderr().lock(),
                    program.as_os_str(),
                    args,
                    CLAUDE_MAX_STATUS_INPUT_BYTES,
                    |captured| {
                        ingest_relayed_claude(
                            context.as_ref().map(|(storage, config)| (storage, config)),
                            captured,
                            observed_at,
                        )
                    },
                )?
            }
            RelayCommand::Antigravity { command } => relay_antigravity(
                context.as_ref().map(|(storage, config)| (storage, config)),
                io::stdin().lock(),
                io::stdout().lock(),
                io::stderr().lock(),
                command,
                observed_at,
            )?,
        };
        return if outcome.exit_code == 0 {
            Ok(())
        } else {
            Err(CliError::WrappedExit(outcome.exit_code))
        };
    }
    let paths = AppPaths::resolve_with_config(cli.data_dir.as_deref(), cli.config_file.as_deref())?;
    let persisted = AppConfig::load(&paths.config_path())?;
    let config = merged_config(persisted, &cli)?;
    let codex_executable = cli.codex_executable.clone();
    let no_codex_poll = cli.no_codex_poll;
    let copilot_runtime = cli.copilot_runtime.clone();
    let no_copilot_poll = cli.no_copilot_poll;
    match cli.command.unwrap_or(Command::Dashboard) {
        Command::Dashboard => {
            let storage = Storage::open(paths.database_path())?;
            run_dashboard(
                storage,
                config,
                paths.config_path(),
                codex_executable,
                copilot_runtime,
                no_codex_poll,
                no_copilot_poll,
            )
        }
        Command::Ingest {
            provider: IngestCommand::Claude,
        } => {
            let storage =
                Storage::open_with_busy_timeout(paths.database_path(), Duration::from_millis(500))?;
            let now = now_millis()?;
            let bell = io::stdout().is_terminal();
            let mut output = io::stdout().lock();
            let snapshots = ingest_claude(&storage, io::stdin().lock(), &mut output, now)?;
            finish_observation(&storage, &snapshots, &config, now, &mut output, bell)
        }
        Command::Ingest {
            provider: IngestCommand::Antigravity,
        } => {
            let storage =
                Storage::open_with_busy_timeout(paths.database_path(), Duration::from_millis(500))?;
            let now = now_millis()?;
            ingest_antigravity(
                &storage,
                io::stdin().lock(),
                io::stdout().lock(),
                now,
                &config,
            )?;
            Ok(())
        }
        Command::Relay { .. } => unreachable!("relay commands return before configuration"),
        Command::Refresh {
            provider: RefreshCommand::Codex { executable },
        } => {
            let storage = Storage::open(paths.database_path())?;
            let probe = CodexAdapter::new(executable);
            let now = now_millis()?;
            let bell = io::stdout().is_terminal();
            let mut output = io::stdout().lock();
            let snapshots = refresh_codex(&storage, &probe, &mut output, now)?;
            finish_observation(&storage, &snapshots, &config, now, &mut output, bell)
        }
        Command::Refresh {
            provider: RefreshCommand::Copilot,
        } => {
            let storage = Storage::open(paths.database_path())?;
            let now = now_millis()?;
            let probe = CopilotAdapter::new(copilot_runtime)
                .map_err(|error| copilot_refresh_error(&storage, now, error.class()))?;
            let bell = io::stdout().is_terminal();
            refresh_copilot(&storage, &probe, io::stdout().lock(), now, &config, bell)?;
            Ok(())
        }
        Command::Watch {
            provider: WatchCommand::Codex { executable },
        } => watch::run_codex(storage_at(&paths)?, executable, config),
        Command::Watch {
            provider: WatchCommand::Copilot,
        } => watch::run_copilot(storage_at(&paths)?, copilot_runtime, config),
        Command::History { limit } => {
            let storage = Storage::open(paths.database_path())?;
            let reference_millis = now_millis()?;
            history::write(&storage, io::stdout().lock(), limit, reference_millis)
        }
        Command::Config {
            action: ConfigCommand::Show,
        } => {
            write!(io::stdout().lock(), "{}", config.to_toml()?)?;
            Ok(())
        }
        Command::Config {
            action: ConfigCommand::Save,
        } => {
            let path = paths.config_path();
            config.save(&path)?;
            writeln!(
                io::stdout().lock(),
                "Saved dashboard settings to {}",
                path.display()
            )?;
            Ok(())
        }
    }
}

fn relay_context(cli: &Cli) -> Option<(Storage, AppConfig)> {
    let paths =
        AppPaths::resolve_with_config(cli.data_dir.as_deref(), cli.config_file.as_deref()).ok()?;
    let persisted = AppConfig::load(&paths.config_path()).ok()?;
    let config = merged_config(persisted, cli).ok()?;
    let storage =
        Storage::open_with_busy_timeout(paths.database_path(), Duration::from_millis(500)).ok()?;
    Some((storage, config))
}

fn run_dashboard(
    storage: Storage,
    config: AppConfig,
    config_path: PathBuf,
    executable: PathBuf,
    copilot_runtime: Option<PathBuf>,
    no_codex_poll: bool,
    no_copilot_poll: bool,
) -> Result<(), CliError> {
    let codex_enabled = !no_codex_poll;
    let copilot_enabled = !no_copilot_poll;
    let poller = DashboardPoller::start(
        storage.clone(),
        executable,
        copilot_runtime,
        config.clone(),
        codex_enabled,
        copilot_enabled,
    )?;
    let dashboard_result = tui::run(storage, config, config_path, codex_enabled, copilot_enabled);
    let poll_result = poller.stop_and_join();
    dashboard_result?;
    poll_result?;
    Ok(())
}

fn storage_at(paths: &AppPaths) -> Result<Storage, CliError> {
    Ok(Storage::open(paths.database_path())?)
}

fn merged_config(mut config: AppConfig, cli: &Cli) -> Result<AppConfig, CliError> {
    if let Some(value) = cli.codex_refresh_seconds {
        config.codex_refresh_seconds = value;
    }
    if let Some(value) = cli.copilot_refresh_seconds {
        config.copilot_refresh_seconds = value;
    }
    if let Some(value) = cli.claude_stale_after_seconds {
        config.claude_stale_after_seconds = value;
    }
    if let Some(value) = cli.antigravity_stale_after_seconds {
        config.antigravity_stale_after_seconds = value;
    }
    if let Some(value) = cli.retention_days {
        config.retention_days = value;
    }
    if let Some(value) = cli.low_remaining_percent {
        config.low_remaining_percent = value;
    }
    if let Some(value) = cli.reset_soon_seconds {
        config.reset_soon_seconds = value;
    }
    if cli.native_notifications {
        config.native_notifications = true;
    }
    if cli.no_native_notifications {
        config.native_notifications = false;
    }
    config.validate()?;
    Ok(config)
}

fn parse_history_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "history limit must be an integer".to_owned())?;
    if (1..=crate::storage::MAX_HISTORY_LIMIT).contains(&limit) {
        Ok(limit)
    } else {
        Err(format!(
            "history limit must be between 1 and {}",
            crate::storage::MAX_HISTORY_LIMIT
        ))
    }
}

pub fn database_path(data_dir: Option<&Path>) -> Result<PathBuf, PathError> {
    Ok(AppPaths::resolve(data_dir)?.database_path())
}

fn now_millis() -> Result<i64, CliError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CliError::Clock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| CliError::Clock)
}

fn ingest_claude<R: Read, W: Write>(
    storage: &Storage,
    reader: R,
    mut writer: W,
    observed_at: i64,
) -> Result<Vec<QuotaSnapshot>, CliError> {
    let mut input = Vec::new();
    reader
        .take((CLAUDE_MAX_STATUS_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    let observation = parse_claude_status_line(&input, observed_at)?;
    let batch = normalize_rows(observation.snapshots);
    storage.insert_snapshots(&batch.accepted)?;
    writeln!(writer, "{}", concise_summary("Claude", &batch.accepted))?;
    Ok(batch.accepted)
}

fn ingest_relayed_claude(
    context: Option<(&Storage, &AppConfig)>,
    captured: CapturedInput<'_>,
    observed_at: i64,
) -> Result<(), ()> {
    let (storage, config, input) = match (context, captured) {
        (Some((storage, config)), CapturedInput::Complete(input)) => (storage, config, input),
        (None, _) | (_, CapturedInput::Overflow) => return Ok(()),
    };
    let observation = parse_claude_status_line(input, observed_at).map_err(|_| ())?;
    let batch = normalize_rows(observation.snapshots);
    storage.insert_snapshots(&batch.accepted).map_err(|_| ())?;
    finish_observation(
        storage,
        &batch.accepted,
        config,
        observed_at,
        io::sink(),
        false,
    )
    .map_err(|_| ())
}

fn ingest_antigravity<R: Read, W: Write>(
    storage: &Storage,
    reader: R,
    mut writer: W,
    observed_at: i64,
    config: &AppConfig,
) -> Result<Vec<QuotaSnapshot>, CliError> {
    storage
        .record_provider_attempt(Provider::GoogleAntigravity, observed_at)
        .map_err(|_| antigravity_ingest_error(storage, observed_at, "storage"))?;

    let mut input = Vec::new();
    if reader
        .take((ANTIGRAVITY_MAX_STATUS_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .is_err()
    {
        return Err(antigravity_ingest_error(storage, observed_at, "parse"));
    }
    let result = persist_antigravity_capture(storage, config, &input, observed_at);
    let (snapshots, rejected) = match result {
        Ok(result) => result,
        Err(class) => return Err(antigravity_ingest_error(storage, observed_at, class)),
    };
    storage
        .record_provider_success(Provider::GoogleAntigravity, observed_at)
        .map_err(|_| antigravity_ingest_error(storage, observed_at, "storage"))?;
    writeln!(
        writer,
        "Google AI · Antigravity: accepted {}, rejected {}",
        snapshots.len(),
        rejected
    )?;
    Ok(snapshots)
}

fn relay_antigravity<R: Read, O: Write, E: Write>(
    context: Option<(&Storage, &AppConfig)>,
    reader: R,
    stdout: O,
    stderr: E,
    command: &[OsString],
    observed_at: i64,
) -> Result<status_line_relay::RelayOutcome, CliError> {
    let (program, args) = command.split_first().expect("clap requires a command");
    if let Some((storage, _)) = context {
        let _ = storage.record_provider_attempt(Provider::GoogleAntigravity, observed_at);
    }

    let mut callback_result = None;
    let relay_result = status_line_relay::relay(
        reader,
        stdout,
        stderr,
        program.as_os_str(),
        args,
        ANTIGRAVITY_MAX_STATUS_INPUT_BYTES,
        |captured| {
            callback_result = Some(ingest_relayed_antigravity(context, captured, observed_at));
            callback_result
                .as_ref()
                .expect("callback result was set")
                .map_or_else(|_| Err(()), |_| Ok(()))
        },
    );

    let outcome = match relay_result {
        Ok(outcome) => outcome,
        Err(error) => {
            record_antigravity_relay_failure(context, observed_at, "wrapped_command");
            return Err(error.into());
        }
    };
    if outcome.exit_code != 0 {
        record_antigravity_relay_failure(context, observed_at, "wrapped_command");
    } else if let Some(result) = callback_result {
        match result {
            Ok(()) => {
                if let Some((storage, _)) = context {
                    let _ =
                        storage.record_provider_success(Provider::GoogleAntigravity, observed_at);
                }
            }
            Err(class) => record_antigravity_relay_failure(context, observed_at, class),
        }
    }
    Ok(outcome)
}

fn ingest_relayed_antigravity(
    context: Option<(&Storage, &AppConfig)>,
    captured: CapturedInput<'_>,
    observed_at: i64,
) -> Result<(), &'static str> {
    let Some((storage, config)) = context else {
        return Ok(());
    };
    let input = match captured {
        CapturedInput::Complete(input) => input,
        CapturedInput::Overflow => return Err("oversize"),
    };
    persist_antigravity_capture(storage, config, input, observed_at).map(|_| ())
}

fn persist_antigravity_capture(
    storage: &Storage,
    config: &AppConfig,
    input: &[u8],
    observed_at: i64,
) -> Result<(Vec<QuotaSnapshot>, usize), &'static str> {
    let observation =
        parse_antigravity_status_line(input, observed_at).map_err(|error| match error {
            AntigravityParseError::Oversized => "oversize",
            AntigravityParseError::InvalidDocument | AntigravityParseError::UnexpectedProduct => {
                "parse"
            }
        })?;
    let parser_rejected = observation.rejected_buckets;
    let batch = normalize_rows(observation.snapshots);
    let rejected = parser_rejected.saturating_add(batch.rejected.len());
    if batch.accepted.is_empty() && rejected > 0 {
        return Err("parse");
    }
    storage
        .insert_snapshots(&batch.accepted)
        .map_err(|_| "storage")?;
    finish_observation(
        storage,
        &batch.accepted,
        config,
        observed_at,
        io::sink(),
        false,
    )
    .map_err(|_| "storage")?;
    Ok((batch.accepted, rejected))
}

fn antigravity_ingest_error(storage: &Storage, attempted_at: i64, class: &'static str) -> CliError {
    let _ = storage.record_provider_failure(Provider::GoogleAntigravity, attempted_at, class);
    CliError::AntigravityIngest { class }
}

fn record_antigravity_relay_failure(
    context: Option<(&Storage, &AppConfig)>,
    attempted_at: i64,
    class: &'static str,
) {
    if let Some((storage, _)) = context {
        let _ = storage.record_provider_failure(Provider::GoogleAntigravity, attempted_at, class);
    }
}

fn refresh_codex<W: Write>(
    storage: &Storage,
    probe: &impl CodexProbe,
    mut writer: W,
    observed_at: i64,
) -> Result<Vec<QuotaSnapshot>, CliError> {
    let observation = probe.read(observed_at)?;
    let batch = normalize_rows(observation.snapshots);
    storage.insert_snapshots(&batch.accepted)?;
    writeln!(writer, "{}", concise_summary("Codex", &batch.accepted))?;
    Ok(batch.accepted)
}

fn refresh_copilot<W: Write>(
    storage: &Storage,
    probe: &impl CopilotProbe,
    mut writer: W,
    observed_at: i64,
    config: &AppConfig,
    bell: bool,
) -> Result<Vec<QuotaSnapshot>, CliError> {
    storage
        .record_provider_attempt(crate::domain::Provider::GitHubCopilot, observed_at)
        .map_err(|_| copilot_refresh_error(storage, observed_at, "storage"))?;
    let observation = probe
        .read(observed_at)
        .map_err(|error| copilot_refresh_error(storage, observed_at, error.class()))?;
    let batch = normalize_rows(observation.snapshots);
    storage
        .insert_snapshots(&batch.accepted)
        .map_err(|_| copilot_refresh_error(storage, observed_at, "storage"))?;
    writeln!(
        writer,
        "{}",
        concise_summary("GitHub Copilot", &batch.accepted)
    )?;
    if let Err(error) = finish_observation(
        storage,
        &batch.accepted,
        config,
        observed_at,
        &mut writer,
        bell,
    ) {
        return match error {
            CliError::Storage(_) => Err(copilot_refresh_error(storage, observed_at, "storage")),
            other => {
                let _ = storage
                    .record_provider_success(crate::domain::Provider::GitHubCopilot, observed_at);
                Err(other)
            }
        };
    }
    storage
        .record_provider_success(crate::domain::Provider::GitHubCopilot, observed_at)
        .map_err(|_| copilot_refresh_error(storage, observed_at, "storage"))?;
    Ok(batch.accepted)
}

fn copilot_refresh_error(storage: &Storage, attempted_at: i64, class: &'static str) -> CliError {
    let _ = storage.record_provider_failure(
        crate::domain::Provider::GitHubCopilot,
        attempted_at,
        class,
    );
    CliError::CopilotRefresh { class }
}

fn finish_observation<W: Write>(
    storage: &Storage,
    snapshots: &[QuotaSnapshot],
    config: &AppConfig,
    now: i64,
    writer: W,
    bell: bool,
) -> Result<(), CliError> {
    let events = AlertEngine::new(config.clone()).process(storage, snapshots, &FixedClock(now))?;
    let mut terminal = TerminalNotifier::new(writer, bell);
    let mut desktop = DesktopNotifier;
    let native = config
        .native_notifications
        .then_some(&mut desktop as &mut dyn crate::notify::Notifier);
    let report = deliver(&events, &mut terminal, native);
    if report.native_failures > 0 {
        let _ = terminal.capability_message();
    }
    let retention_millis = i64::from(config.retention_days).saturating_mul(24 * 60 * 60 * 1_000);
    let cutoff = now.saturating_sub(retention_millis);
    storage.prune_before(cutoff)?;
    storage.prune_alerts_before(cutoff)?;
    Ok(())
}

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

fn concise_summary(provider: &str, snapshots: &[QuotaSnapshot]) -> String {
    if snapshots.is_empty() {
        return format!("{provider}: no quota windows");
    }
    let windows = snapshots
        .iter()
        .map(|snapshot| {
            let row = snapshot.as_input();
            if row.limit_kind == crate::domain::LimitKind::Unlimited {
                return format!("{} Unlimited", window_label(row));
            }
            format!(
                "{} {:.1}% used, {:.1}% left",
                window_label(row),
                row.used_percent,
                snapshot.remaining_percent()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{provider}: {windows}")
}

fn window_label(row: &QuotaSnapshotInput) -> String {
    match row.window_kind {
        crate::domain::WindowKind::Rolling5h => "5h".into(),
        crate::domain::WindowKind::Rolling7d => "7d".into(),
        crate::domain::WindowKind::Spend => "spend".into(),
        crate::domain::WindowKind::Monthly => humanize_scope(&row.scope_key),
        crate::domain::WindowKind::Other => row
            .window_duration_seconds
            .map(|seconds| format!("{}m", seconds / 60))
            .unwrap_or_else(|| "other".into()),
    }
}

fn humanize_scope(scope_key: &str) -> String {
    let label = scope_key.replace('_', " ");
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => label,
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
