use std::{io, path::PathBuf};

use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use thiserror::Error;

use crate::{
    config::{AppConfig, ProviderPreferences},
    domain::{Availability, LimitKind, Provider},
    storage::{Storage, StoredSnapshot},
    time_format::{format_age_millis, format_duration_seconds, format_snapshot_reset},
};

#[path = "tui_format.rs"]
mod format;
use format::{format_usage, humanize_scope, provider_label, usage_color, window_label};

#[path = "tui_runtime.rs"]
mod runtime;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal operation failed")]
    Terminal(#[from] io::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("provider preferences could not be saved")]
    PreferencesSave,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowView {
    pub provider: Provider,
    pub window: String,
    pub scope: String,
    pub used: String,
    pub used_percent: f64,
    pub limited: bool,
    pub reset: String,
    pub age: String,
    pub state: String,
    pub stale: bool,
}

const PROVIDERS: [Provider; 4] = [
    Provider::Codex,
    Provider::Claude,
    Provider::GitHubCopilot,
    Provider::GoogleAntigravity,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEntry {
    pub provider: Provider,
    pub visible: bool,
}

#[derive(Debug)]
pub struct DashboardState {
    pub windows: Vec<WindowView>,
    pub error: Option<String>,
    pub poll_status: String,
    pub recent_alert: Option<String>,
    pub providers: Vec<ProviderEntry>,
    pub selected_provider: usize,
    pub managing_providers: bool,
    preferences_changed: bool,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            error: None,
            poll_status: String::new(),
            recent_alert: None,
            providers: PROVIDERS
                .into_iter()
                .map(|provider| ProviderEntry {
                    provider,
                    visible: true,
                })
                .collect(),
            selected_provider: 0,
            managing_providers: false,
            preferences_changed: false,
        }
    }
}

impl DashboardState {
    pub fn load(
        storage: &Storage,
        config: &AppConfig,
        now_millis: i64,
        codex_polling_enabled: bool,
        copilot_polling_enabled: bool,
    ) -> Self {
        match storage.latest_snapshots() {
            Ok(snapshots) => {
                let poll_status = poll_status(
                    storage,
                    &snapshots,
                    now_millis,
                    codex_polling_enabled,
                    copilot_polling_enabled,
                );
                Self {
                    windows: project_windows(&snapshots, config, now_millis),
                    error: None,
                    poll_status,
                    recent_alert: recent_alert(storage),
                    providers: provider_entries(&config.provider_preferences),
                    ..Self::default()
                }
            }
            Err(_) => Self {
                windows: Vec::new(),
                error: Some(
                    "Latest snapshots are unavailable; the database could not be read.".into(),
                ),
                poll_status: "Provider status unavailable".into(),
                recent_alert: None,
                providers: provider_entries(&config.provider_preferences),
                ..Self::default()
            },
        }
    }

    pub fn provider_preferences(&self) -> ProviderPreferences {
        ProviderPreferences {
            order: self
                .providers
                .iter()
                .map(|entry| entry.provider.as_str().to_owned())
                .collect(),
            hidden: self
                .providers
                .iter()
                .filter(|entry| !entry.visible)
                .map(|entry| entry.provider.as_str().to_owned())
                .collect(),
        }
    }

    pub fn preferences_changed(&self) -> bool {
        self.preferences_changed
    }

    pub fn mark_preferences_saved(&mut self) {
        self.preferences_changed = false;
    }

    fn selected_entry_mut(&mut self) -> Option<&mut ProviderEntry> {
        self.providers.get_mut(self.selected_provider)
    }

    fn move_selected(&mut self, earlier: bool) {
        let Some(selected) = self.providers.get(self.selected_provider) else {
            return;
        };
        if !selected.visible {
            return;
        }
        let adjacent = if earlier {
            (0..self.selected_provider)
                .rev()
                .find(|index| self.providers[*index].visible)
        } else {
            (self.selected_provider + 1..self.providers.len())
                .find(|index| self.providers[*index].visible)
        };
        if let Some(adjacent) = adjacent {
            self.providers.swap(self.selected_provider, adjacent);
            self.selected_provider = adjacent;
            self.preferences_changed = true;
        }
    }
}

fn provider_entries(preferences: &ProviderPreferences) -> Vec<ProviderEntry> {
    preferences
        .reconcile(PROVIDERS.map(Provider::as_str))
        .into_iter()
        .filter_map(|entry| {
            PROVIDERS
                .into_iter()
                .find(|provider| provider.as_str() == entry.key)
                .map(|provider| ProviderEntry {
                    provider,
                    visible: entry.visible,
                })
        })
        .collect()
}

fn poll_status(
    storage: &Storage,
    snapshots: &[StoredSnapshot],
    now_millis: i64,
    codex_enabled: bool,
    copilot_enabled: bool,
) -> String {
    let codex = provider_poll_status(storage, Provider::Codex, "Codex", now_millis, codex_enabled);
    let claude = provider_ingest_status(
        storage,
        Provider::Claude,
        "Claude",
        latest_observed_at(snapshots, Provider::Claude),
        now_millis,
    );
    let copilot = provider_poll_status(
        storage,
        Provider::GitHubCopilot,
        "Copilot",
        now_millis,
        copilot_enabled,
    );
    let antigravity = provider_ingest_status(
        storage,
        Provider::GoogleAntigravity,
        "Antigravity",
        latest_observed_at(snapshots, Provider::GoogleAntigravity),
        now_millis,
    );
    format!("Poll: {codex} · {copilot} | Ingest: {claude} · {antigravity}")
}

fn latest_observed_at(snapshots: &[StoredSnapshot], provider: Provider) -> Option<i64> {
    snapshots
        .iter()
        .filter(|stored| stored.snapshot.as_input().provider == provider)
        .map(|stored| stored.snapshot.as_input().observed_at)
        .max()
}

fn provider_ingest_status(
    storage: &Storage,
    provider: Provider,
    label: &str,
    latest_observed_at: Option<i64>,
    now_millis: i64,
) -> String {
    match storage.provider_state(provider) {
        Ok(Some(state)) if state.consecutive_failures > 0 => format!(
            "{label} {} ×{}",
            sanitized_ingest_error(state.last_error_class.as_deref()),
            state.consecutive_failures
        ),
        Ok(Some(state)) => state.last_success_at.map_or_else(
            || format!("{label} waiting"),
            |at| format!("{label} ok {}", format_age_millis(at, now_millis)),
        ),
        Ok(None) => latest_observed_at.map_or_else(
            || format!("{label} waiting"),
            |at| format!("{label} seen {}", format_age_millis(at, now_millis)),
        ),
        Err(_) => format!("{label} unavailable"),
    }
}

fn sanitized_ingest_error(error_class: Option<&str>) -> &'static str {
    match error_class {
        Some("oversize") => "oversize",
        Some("parse") => "parse",
        Some("storage") => "storage",
        Some("wrapped_command") => "wrapped_command",
        _ => "failed",
    }
}

fn provider_poll_status(
    storage: &Storage,
    provider: Provider,
    label: &str,
    now_millis: i64,
    enabled: bool,
) -> String {
    if !enabled {
        return format!("{label} off");
    }
    match storage.provider_state(provider) {
        Ok(Some(state)) if state.consecutive_failures > 0 => format!(
            "{label} {} ×{}",
            state.last_error_class.as_deref().unwrap_or("failed"),
            state.consecutive_failures
        ),
        Ok(Some(state)) => state.last_success_at.map_or_else(
            || format!("{label} starting"),
            |at| format!("{label} ok {}", format_age_millis(at, now_millis)),
        ),
        Ok(None) => format!("{label} starting"),
        Err(_) => format!("{label} unavailable"),
    }
}

fn recent_alert(storage: &Storage) -> Option<String> {
    storage
        .recent_alerts(1)
        .ok()?
        .into_iter()
        .next()
        .map(|alert| {
            format!(
                "Latest alert: {} {} {}",
                provider_label(alert.provider),
                window_label(alert.window_kind, None),
                alert.alert_kind
            )
        })
}

pub fn project_windows(
    snapshots: &[StoredSnapshot],
    config: &AppConfig,
    now_millis: i64,
) -> Vec<WindowView> {
    let mut windows: Vec<_> = snapshots
        .iter()
        .map(|stored| {
            let snapshot = &stored.snapshot;
            let row = snapshot.as_input();
            let age_millis = now_millis.saturating_sub(row.observed_at).max(0);
            let age_seconds = age_millis / 1_000;
            let stale_after = match row.provider {
                Provider::Codex => (config.codex_refresh_seconds * 2).max(180),
                Provider::Claude => config.claude_stale_after_seconds,
                Provider::GitHubCopilot => (config.copilot_refresh_seconds * 2).max(180),
                Provider::GoogleAntigravity => config.antigravity_stale_after_seconds,
            } as i64;
            let reset_expired = row
                .resets_at
                .is_some_and(|reset| reset <= now_millis / 1_000);
            let stale = age_millis > stale_after.saturating_mul(1_000) || reset_expired;
            let stale_marker = if stale { " · stale" } else { "" };
            WindowView {
                provider: row.provider,
                window: window_label(row.window_kind, row.window_duration_seconds),
                scope: humanize_scope(&row.scope_key),
                used: match row.limit_kind {
                    LimitKind::Limited => format_usage(row.used_percent),
                    LimitKind::Unlimited => "Unlimited".into(),
                },
                used_percent: row.used_percent,
                limited: row.limit_kind == LimitKind::Limited,
                reset: format_snapshot_reset(
                    row.provider,
                    row.window_kind,
                    row.resets_at,
                    row.observed_at,
                    now_millis / 1_000,
                ),
                age: format_duration_seconds(age_seconds),
                state: match row.availability {
                    Availability::Unknown => format!("{}{}", row.quality.as_str(), stale_marker),
                    availability => format!(
                        "{} · {}{}",
                        row.quality.as_str(),
                        availability.as_str(),
                        stale_marker
                    ),
                },
                stale,
            }
        })
        .collect();
    windows.sort_by_key(|window| provider_order(window.provider));
    windows
}

fn provider_order(provider: Provider) -> u8 {
    match provider {
        Provider::Codex => 0,
        Provider::Claude => 1,
        Provider::GitHubCopilot => 2,
        Provider::GoogleAntigravity => 3,
    }
}

pub fn render(frame: &mut Frame<'_>, state: &DashboardState) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(frame.area());
    let overview = Paragraph::new(Line::raw(state.poll_status.clone())).block(
        Block::default()
            .title(" Usage overview ")
            .borders(Borders::ALL),
    );
    frame.render_widget(overview, areas[0]);

    if state.managing_providers {
        render_provider_manager(frame, areas[1], state);
    } else if state.providers.iter().all(|provider| !provider.visible) {
        frame.render_widget(
            Paragraph::new("No providers are visible. Press m to manage and restore providers.")
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .title(" Provider windows ")
                        .borders(Borders::ALL),
                ),
            areas[1],
        );
    } else if let Some(message) = &state.error {
        frame.render_widget(
            Paragraph::new(message.as_str())
                .style(Style::default().fg(Color::Red))
                .block(Block::default().title(" Error ").borders(Borders::ALL)),
            areas[1],
        );
    } else if state.windows.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "No quota snapshots yet. Waiting for Codex/Copilot polling or Claude/Antigravity ingestion.",
            )
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .title(" Provider windows ")
                    .borders(Borders::ALL),
            ),
            areas[1],
        );
    } else {
        let providers: Vec<_> = state
            .providers
            .iter()
            .filter(|entry| entry.visible)
            .map(|entry| entry.provider)
            .filter(|provider| {
                state
                    .windows
                    .iter()
                    .any(|window| window.provider == *provider)
            })
            .collect();
        let panel_areas =
            Layout::vertical(vec![Constraint::Fill(1); providers.len()]).split(areas[1]);

        for (provider, area) in providers.into_iter().zip(panel_areas.iter().copied()) {
            render_provider_panel(frame, area, provider, &state.windows);
        }
    }
    frame.render_widget(
        Paragraph::new(format!(
            "{}\n{}",
            state
                .recent_alert
                .as_deref()
                .unwrap_or("No alerts recorded"),
            if state.managing_providers {
                "Up/Down or j/k select | [/] move visible | v hide/restore | m/Esc done | q quit"
            } else {
                "m manage providers | q / Esc quit"
            }
        ))
        .style(Style::default().fg(Color::DarkGray)),
        areas[2],
    );
}

fn render_provider_manager(frame: &mut Frame<'_>, area: Rect, state: &DashboardState) {
    let lines: Vec<_> = state
        .providers
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let selected = if index == state.selected_provider {
                ">"
            } else {
                " "
            };
            let visibility = if entry.visible { "visible" } else { "hidden" };
            Line::raw(format!(
                "{selected} {:<26} [{visibility}]",
                provider_label(entry.provider)
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Manage providers ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_provider_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    provider: Provider,
    windows: &[WindowView],
) {
    let rows = windows
        .iter()
        .filter(|window| window.provider == provider)
        .map(|window| {
            let style = if window.stale {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Row::new([
                Cell::from(window.window.clone()),
                Cell::from(window.scope.clone()),
                if window.limited {
                    Cell::from(window.used.clone())
                        .style(Style::default().fg(usage_color(window.used_percent)))
                } else {
                    Cell::from(window.used.clone())
                },
                Cell::from(window.reset.clone()),
                Cell::from(window.age.clone()),
                Cell::from(window.state.clone()),
            ])
            .style(style)
        });
    let header = Row::new(["Window", "Scope", "Usage", "Reset", "Age", "State"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(24),
            Constraint::Length(43),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(24),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .title(format!(" {} windows ", provider_label(provider)))
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

pub fn should_quit(event: &Event) -> bool {
    matches!(event, Event::Key(key)
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardAction {
    Continue,
    Quit,
}

pub fn handle_event(state: &mut DashboardState, event: &Event) -> DashboardAction {
    let Event::Key(key) = event else {
        return DashboardAction::Continue;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return DashboardAction::Continue;
    }
    if key.code == KeyCode::Char('q') {
        return DashboardAction::Quit;
    }
    if !state.managing_providers {
        return match key.code {
            KeyCode::Esc => DashboardAction::Quit,
            KeyCode::Char('m') => {
                state.managing_providers = true;
                DashboardAction::Continue
            }
            _ => DashboardAction::Continue,
        };
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('m') => state.managing_providers = false,
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected_provider = state.selected_provider.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected_provider =
                (state.selected_provider + 1).min(state.providers.len().saturating_sub(1));
        }
        KeyCode::Char('[') => state.move_selected(true),
        KeyCode::Char(']') => state.move_selected(false),
        KeyCode::Char('v') => {
            if let Some(entry) = state.selected_entry_mut() {
                entry.visible = !entry.visible;
                state.preferences_changed = true;
            }
        }
        _ => {}
    }
    DashboardAction::Continue
}

pub fn run(
    storage: Storage,
    config: AppConfig,
    config_path: PathBuf,
    codex_polling_enabled: bool,
    copilot_polling_enabled: bool,
) -> Result<(), TuiError> {
    runtime::run(
        storage,
        config,
        config_path,
        codex_polling_enabled,
        copilot_polling_enabled,
    )
}

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
