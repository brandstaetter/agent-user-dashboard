use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use super::{
    CliError, CopilotProbe, copilot_refresh_error, finish_observation, now_millis, refresh_codex,
    refresh_copilot,
};
use crate::{
    config::AppConfig,
    providers::{codex::CodexAdapter, github_copilot::CopilotAdapter},
    storage::Storage,
};

pub(super) fn run_codex(
    storage: Storage,
    executable: PathBuf,
    config: AppConfig,
) -> Result<(), CliError> {
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))?;
    let probe = CodexAdapter::new(executable);
    let bell = io::stdout().is_terminal();
    let mut output = io::stdout().lock();
    writeln!(
        output,
        "Watching Codex every {} seconds; press Ctrl+C to stop.",
        config.codex_refresh_seconds
    )?;
    while running.load(Ordering::SeqCst) {
        let now = now_millis()?;
        match refresh_codex(&storage, &probe, &mut output, now) {
            Ok(snapshots) => {
                finish_observation(&storage, &snapshots, &config, now, &mut output, bell)?
            }
            Err(error) => writeln!(output, "Codex refresh failed: {error}")?,
        }
        wait_for_interval(&running, Duration::from_secs(config.codex_refresh_seconds));
    }
    writeln!(output, "Codex watch stopped.")?;
    Ok(())
}

pub(super) fn run_copilot(
    storage: Storage,
    runtime_path: Option<PathBuf>,
    config: AppConfig,
) -> Result<(), CliError> {
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))?;
    let now = now_millis()?;
    let probe = CopilotAdapter::new(runtime_path)
        .map_err(|error| copilot_refresh_error(&storage, now, error.class()))?;
    let bell = io::stdout().is_terminal();
    run_copilot_loop(
        &storage,
        &probe,
        &config,
        running.as_ref(),
        io::stdout().lock(),
        now_millis,
        wait_for_interval,
        bell,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_copilot_loop<P, W, N, F>(
    storage: &Storage,
    probe: &P,
    config: &AppConfig,
    running: &AtomicBool,
    mut output: W,
    mut now: N,
    mut wait: F,
    bell: bool,
) -> Result<(), CliError>
where
    P: CopilotProbe,
    W: Write,
    N: FnMut() -> Result<i64, CliError>,
    F: FnMut(&AtomicBool, Duration),
{
    writeln!(
        output,
        "Watching GitHub Copilot every {} seconds; press Ctrl+C to stop.",
        config.copilot_refresh_seconds
    )?;
    while running.load(Ordering::SeqCst) {
        let observed_at = now()?;
        if let Err(error) = refresh_copilot(storage, probe, &mut output, observed_at, config, bell)
        {
            writeln!(output, "{error}")?;
        }
        wait(running, Duration::from_secs(config.copilot_refresh_seconds));
    }
    writeln!(output, "GitHub Copilot watch stopped.")?;
    Ok(())
}

fn wait_for_interval(running: &AtomicBool, duration: Duration) {
    let mut remaining = duration;
    while running.load(Ordering::SeqCst) && !remaining.is_zero() {
        let step = remaining.min(Duration::from_millis(250));
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}
