use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    cursor::{Hide, Show},
    event, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::{DashboardAction, DashboardState, TuiError, handle_event, render};
use crate::{config::AppConfig, storage::Storage};

pub(super) fn run(
    storage: Storage,
    config: AppConfig,
    config_path: PathBuf,
    codex_polling_enabled: bool,
    copilot_polling_enabled: bool,
) -> Result<(), TuiError> {
    let mut session = TerminalSession::enter()?;
    let result = run_loop(
        &mut session.terminal,
        &storage,
        &config,
        &config_path,
        codex_polling_enabled,
        copilot_polling_enabled,
    );
    let restore = session.restore();
    result.and(restore)
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    storage: &Storage,
    config: &AppConfig,
    config_path: &Path,
    codex_polling_enabled: bool,
    copilot_polling_enabled: bool,
) -> Result<(), TuiError> {
    let mut state = DashboardState::load(
        storage,
        config,
        now_millis()?,
        codex_polling_enabled,
        copilot_polling_enabled,
    );
    let mut last_load = Instant::now();
    loop {
        terminal.draw(|frame| render(frame, &state))?;
        if event::poll(Duration::from_millis(250))? {
            let action = handle_event(&mut state, &event::read()?);
            persist_provider_preferences(&mut state, config_path)?;
            if action == DashboardAction::Quit {
                return Ok(());
            }
        }
        if last_load.elapsed() >= Duration::from_secs(1) {
            let refreshed = DashboardState::load(
                storage,
                config,
                now_millis()?,
                codex_polling_enabled,
                copilot_polling_enabled,
            );
            state.windows = refreshed.windows;
            state.error = refreshed.error;
            state.poll_status = refreshed.poll_status;
            state.recent_alert = refreshed.recent_alert;
            last_load = Instant::now();
        }
    }
}

pub(super) fn persist_provider_preferences(
    state: &mut DashboardState,
    config_path: &Path,
) -> Result<(), TuiError> {
    if !state.preferences_changed() {
        return Ok(());
    }
    AppConfig::persist_provider_preferences(config_path, state.provider_preferences())
        .map_err(|_| TuiError::PreferencesSave)?;
    state.mark_preferences_saved();
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            cleanup_failed_entry(&mut stdout);
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                active: true,
            }),
            Err(error) => {
                cleanup_failed_entry(&mut io::stdout());
                Err(error)
            }
        }
    }

    fn restore(&mut self) -> Result<(), TuiError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let leave_result = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let show_result = execute!(self.terminal.backend_mut(), Show);
        let raw_result = disable_raw_mode();
        leave_result
            .and(show_result)
            .and(raw_result)
            .map_err(TuiError::from)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn cleanup_failed_entry(stdout: &mut impl io::Write) {
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = execute!(stdout, Show);
    let _ = disable_raw_mode();
}

fn now_millis() -> Result<i64, TuiError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TuiError::Clock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| TuiError::Clock)
}
