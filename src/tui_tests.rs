use std::fs;

use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, style::Color};
use tempfile::tempdir;
use uuid::Uuid;

use super::*;
use crate::domain::{
    Availability, LimitKind, Quality, QuotaSnapshot, QuotaSnapshotInput, WindowKind,
};

const NOW: i64 = 1_700_000_000_000;

fn stored(
    id: i64,
    provider: Provider,
    kind: WindowKind,
    used: f64,
    reset: Option<i64>,
    observed_at: i64,
    quality: Quality,
) -> StoredSnapshot {
    stored_with_limit(
        id,
        provider,
        "subscription",
        kind,
        LimitKind::Limited,
        used,
        reset,
        observed_at,
        quality,
    )
}

#[allow(clippy::too_many_arguments)]
fn stored_with_limit(
    id: i64,
    provider: Provider,
    scope_key: &str,
    kind: WindowKind,
    limit_kind: LimitKind,
    used: f64,
    reset: Option<i64>,
    observed_at: i64,
    quality: Quality,
) -> StoredSnapshot {
    StoredSnapshot {
        id,
        snapshot: QuotaSnapshot::new(QuotaSnapshotInput {
            provider,
            scope_key: scope_key.into(),
            window_kind: kind,
            limit_kind,
            window_duration_seconds: match kind {
                WindowKind::Rolling5h => Some(18_000),
                WindowKind::Rolling7d => Some(604_800),
                _ => None,
            },
            used_percent: used,
            resets_at: reset,
            observed_at,
            availability: Availability::Allowed,
            source_version: None,
            quality,
            source_sequence: Uuid::new_v4(),
        })
        .unwrap(),
    }
}

#[allow(clippy::too_many_arguments)]
fn stored_antigravity(
    id: i64,
    scope_key: &str,
    used: f64,
    reset: Option<i64>,
    observed_at: i64,
    availability: Availability,
    quality: Quality,
) -> StoredSnapshot {
    StoredSnapshot {
        id,
        snapshot: QuotaSnapshot::new(QuotaSnapshotInput {
            provider: Provider::GoogleAntigravity,
            scope_key: scope_key.into(),
            window_kind: WindowKind::Other,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: None,
            used_percent: used,
            resets_at: reset,
            observed_at,
            availability,
            source_version: Some("1.2.3".into()),
            quality,
            source_sequence: Uuid::new_v4(),
        })
        .unwrap(),
    }
}

fn rendered_at(state: &DashboardState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn rendered(state: &DashboardState) -> String {
    rendered_at(state, 160, 14)
}

fn rendered_lines(state: &DashboardState, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|line| line.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn renders_nonblank_cell_with_color(state: &DashboardState, color: Color) -> bool {
    let backend = TestBackend::new(120, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .any(|cell| cell.fg == color && cell.symbol() != " ")
}

#[test]
fn projects_usage_bars_with_used_and_remaining_values() {
    let rows = vec![
        stored(
            1,
            Provider::Codex,
            WindowKind::Rolling5h,
            25.0,
            Some(1_700_003_600),
            NOW - 30_000,
            Quality::Fresh,
        ),
        stored(
            2,
            Provider::Claude,
            WindowKind::Rolling7d,
            42.5,
            None,
            NOW - 60_000,
            Quality::Partial,
        ),
    ];
    let views = project_windows(&rows, &AppConfig::default(), NOW);
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].provider, Provider::Codex);
    assert_eq!(views[1].provider, Provider::Claude);
    assert_eq!(views[0].used, "[████░░░░░░░░░░░░] 25.0% used, 75.0% left");
    assert_eq!(views[0].reset, "in 1h 0m");
    assert_eq!(views[0].age, "30s");
    assert_eq!(views[0].state, "fresh · allowed");
    assert_eq!(views[1].used, "[███████░░░░░░░░░] 42.5% used, 57.5% left");
    assert_eq!(views[1].reset, "unknown");
    assert_eq!(views[1].state, "partial · allowed");

    let screen = rendered(&DashboardState {
        windows: views,
        error: None,
        ..DashboardState::default()
    });
    for expected in [
        "Codex",
        "Claude",
        "5 hours",
        "7 days",
        "25.0% used, 75.0% left",
        "42.5% used, 57.5% left",
        "unknown",
        "partial",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} from {screen:?}"
        );
    }
    assert!(!screen.contains("Left"));
    assert!(!screen.contains("1700003600"));
}

#[test]
fn unknown_availability_is_omitted_from_state() {
    let row = StoredSnapshot {
        id: 1,
        snapshot: QuotaSnapshot::new(QuotaSnapshotInput {
            provider: Provider::Codex,
            scope_key: "subscription".into(),
            window_kind: WindowKind::Rolling5h,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: Some(18_000),
            used_percent: 25.0,
            resets_at: Some(NOW / 1_000 + 3_600),
            observed_at: NOW,
            availability: Availability::Unknown,
            source_version: None,
            quality: Quality::Fresh,
            source_sequence: Uuid::new_v4(),
        })
        .unwrap(),
    };

    let views = project_windows(&[row], &AppConfig::default(), NOW);
    assert_eq!(views[0].state, "fresh");

    let screen = rendered(&DashboardState {
        windows: views,
        ..DashboardState::default()
    });
    assert!(!screen.contains("unknown"));
}

#[test]
fn one_provider_renders_one_titled_bordered_panel_without_provider_column() {
    let views = project_windows(
        &[stored(
            1,
            Provider::Codex,
            WindowKind::Rolling5h,
            25.0,
            Some(1_700_003_600),
            NOW - 30_000,
            Quality::Fresh,
        )],
        &AppConfig::default(),
        NOW,
    );
    let screen = rendered(&DashboardState {
        windows: views,
        error: None,
        poll_status:
            "Poll: Codex ok 30s ago · Copilot starting | Ingest: Claude waiting · Antigravity waiting"
                .into(),
        recent_alert: Some("Latest alert: Codex 5 hours warning".into()),
        ..DashboardState::default()
    });

    assert!(screen.contains("┌ Codex windows "));
    assert!(screen.contains("┐"));
    assert!(screen.contains("└"));
    assert!(screen.contains("┘"));
    assert!(screen.contains("5 hours"));
    assert!(screen.contains("Usage overview"));
    assert!(screen.contains("Poll: Codex ok 30s ago"));
    assert!(!screen.contains("Codex 1 windows"));
    assert!(screen.contains("Latest alert: Codex 5 hours warning"));
    assert!(screen.contains("q / Esc quit"));
    assert!(!screen.contains("Claude windows"));
    assert!(!screen.contains("Provider"));
}

#[test]
fn provider_panels_are_ordered_and_own_only_their_rows() {
    let mut views = project_windows(
        &[
            stored(
                1,
                Provider::Claude,
                WindowKind::Rolling7d,
                42.5,
                None,
                NOW - 60_000,
                Quality::Partial,
            ),
            stored(
                2,
                Provider::Codex,
                WindowKind::Rolling5h,
                25.0,
                Some(1_700_003_600),
                NOW - 30_000,
                Quality::Fresh,
            ),
        ],
        &AppConfig::default(),
        NOW,
    );
    views
        .iter_mut()
        .find(|view| view.provider == Provider::Claude)
        .unwrap()
        .scope = "claude-only".into();
    views
        .iter_mut()
        .find(|view| view.provider == Provider::Codex)
        .unwrap()
        .scope = "codex-only".into();
    let lines = rendered_lines(
        &DashboardState {
            windows: views,
            error: None,
            ..DashboardState::default()
        },
        120,
        14,
    );
    let find_line = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} from {lines:?}"))
    };
    let codex_title = find_line("Codex windows");
    let codex_row = find_line("codex-only");
    let claude_title = find_line("Claude windows");
    let claude_row = find_line("claude-only");

    assert!(codex_title < codex_row);
    assert!(codex_row < claude_title);
    assert!(claude_title < claude_row);
    assert!(
        lines[codex_title..claude_title]
            .iter()
            .all(|line| !line.contains("claude-only"))
    );
    assert!(
        lines[claude_title..]
            .iter()
            .all(|line| !line.contains("codex-only"))
    );
    assert!(lines.iter().all(|line| !line.contains("Provider")));
}

#[test]
fn usage_colors_change_only_after_requested_thresholds() {
    assert_eq!(usage_color(0.0), Color::Green);
    assert_eq!(usage_color(50.0), Color::Green);
    assert_eq!(usage_color(50.1), Color::Yellow);
    assert_eq!(usage_color(75.0), Color::Yellow);
    assert_eq!(usage_color(75.1), Color::Rgb(255, 165, 0));
    assert_eq!(usage_color(90.0), Color::Rgb(255, 165, 0));
    assert_eq!(usage_color(90.1), Color::Red);
    assert_eq!(
        format_usage(100.0),
        "[████████████████] 100.0% used, 0.0% left"
    );
    assert_eq!(
        format_usage(125.0),
        "[████████████████] 125.0% used, 0.0% left"
    );
}

#[test]
fn copilot_panel_follows_codex_and_claude_and_distinguishes_unlimited() {
    let views = project_windows(
        &[
            stored_with_limit(
                1,
                Provider::GitHubCopilot,
                "premium_interactions",
                WindowKind::Monthly,
                LimitKind::Limited,
                25.0,
                Some(NOW / 1_000 + 3_600),
                NOW - 30_000,
                Quality::Fresh,
            ),
            stored_with_limit(
                2,
                Provider::GitHubCopilot,
                "chat",
                WindowKind::Monthly,
                LimitKind::Unlimited,
                0.0,
                Some(NOW / 1_000 + 3_600),
                NOW - 30_000,
                Quality::Fresh,
            ),
            stored(
                3,
                Provider::Claude,
                WindowKind::Rolling7d,
                42.5,
                None,
                NOW - 60_000,
                Quality::Partial,
            ),
            stored(
                4,
                Provider::Codex,
                WindowKind::Rolling5h,
                10.0,
                Some(NOW / 1_000 + 7_200),
                NOW - 10_000,
                Quality::Fresh,
            ),
        ],
        &AppConfig::default(),
        NOW,
    );
    let state = DashboardState {
        windows: views.clone(),
        poll_status:
            "Poll: Codex starting · Copilot protocol ×1 | Ingest: Claude waiting · Antigravity waiting"
                .into(),
        ..DashboardState::default()
    };
    let lines = rendered_lines(&state, 180, 24);
    let find_line = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} from {lines:?}"))
    };

    assert!(find_line("Codex windows") < find_line("Claude windows"));
    assert!(find_line("Claude windows") < find_line("GitHub Copilot windows"));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Premium interactions"))
    );
    assert!(lines.iter().any(|line| line.contains("Unlimited")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("25.0% used, 75.0% left"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Copilot protocol ×1"))
    );
    let unlimited = views.iter().find(|view| view.scope == "Chat").unwrap();
    assert_eq!(unlimited.used, "Unlimited");
    assert!(!unlimited.limited);
    assert!(!unlimited.used.contains('%'));
}

#[test]
fn antigravity_panel_follows_three_existing_providers_with_dynamic_rows() {
    let views = project_windows(
        &[
            stored_antigravity(
                1,
                "pro_model",
                100.0,
                Some(NOW / 1_000 + 3_600),
                NOW - 30_000,
                Availability::Blocked,
                Quality::Fresh,
            ),
            stored(
                2,
                Provider::Claude,
                WindowKind::Rolling7d,
                42.5,
                None,
                NOW - 60_000,
                Quality::Partial,
            ),
            stored_with_limit(
                3,
                Provider::GitHubCopilot,
                "premium_interactions",
                WindowKind::Monthly,
                LimitKind::Limited,
                25.0,
                Some(NOW / 1_000 + 3_600),
                NOW - 30_000,
                Quality::Fresh,
            ),
            stored(
                4,
                Provider::Codex,
                WindowKind::Rolling5h,
                10.0,
                Some(NOW / 1_000 + 7_200),
                NOW - 10_000,
                Quality::Fresh,
            ),
            stored_antigravity(
                5,
                "gemini-weekly",
                40.0,
                None,
                NOW - 30_000,
                Availability::Allowed,
                Quality::Partial,
            ),
        ],
        &AppConfig::default(),
        NOW,
    );

    assert_eq!(
        views.iter().map(|view| view.provider).collect::<Vec<_>>(),
        vec![
            Provider::Codex,
            Provider::Claude,
            Provider::GitHubCopilot,
            Provider::GoogleAntigravity,
            Provider::GoogleAntigravity,
        ]
    );
    let state = DashboardState {
        windows: views,
        poll_status:
            "Poll: Codex starting · Copilot starting | Ingest: Claude waiting · Antigravity ok 30s ago"
                .into(),
        ..DashboardState::default()
    };
    let lines = rendered_lines(&state, 220, 30);
    let find_line = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} from {lines:?}"))
    };

    assert!(find_line("Codex windows") < find_line("Claude windows"));
    assert!(find_line("Claude windows") < find_line("GitHub Copilot windows"));
    assert!(find_line("GitHub Copilot windows") < find_line("Google AI · Antigravity windows"));
    assert!(lines.iter().any(|line| line.contains("Pro model")));
    assert!(lines.iter().any(|line| line.contains("Gemini weekly")));
    assert!(lines.iter().any(|line| line.contains("fresh · blocked")));
    assert!(lines.iter().any(|line| line.contains("partial · allowed")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Antigravity ok 30s ago"))
    );
    assert!(lines.iter().all(|line| !line.contains("pro_model")));
    assert!(lines.iter().all(|line| !line.contains("gemini-weekly")));

    let overview = lines.iter().find(|line| line.contains("Poll:")).unwrap();
    assert!(overview.contains("Ingest:"));
    assert!(!overview.contains("windows"));
}

#[test]
fn reset_presentation_distinguishes_expected_provider_and_unknown_values() {
    let views = project_windows(
        &[
            stored_with_limit(
                1,
                Provider::GitHubCopilot,
                "missing_reset",
                WindowKind::Monthly,
                LimitKind::Limited,
                25.0,
                None,
                NOW,
                Quality::Partial,
            ),
            stored_with_limit(
                2,
                Provider::GitHubCopilot,
                "provider_reset",
                WindowKind::Monthly,
                LimitKind::Limited,
                25.0,
                Some(NOW / 1_000 + 3_600),
                NOW,
                Quality::Fresh,
            ),
            stored_with_limit(
                3,
                Provider::Codex,
                "codex_missing",
                WindowKind::Rolling5h,
                LimitKind::Limited,
                25.0,
                None,
                NOW,
                Quality::Partial,
            ),
        ],
        &AppConfig::default(),
        NOW,
    );
    let missing = views
        .iter()
        .find(|view| view.scope == "Missing reset")
        .unwrap();
    let provider = views
        .iter()
        .find(|view| view.scope == "Provider reset")
        .unwrap();
    let unrelated = views
        .iter()
        .find(|view| view.scope == "Codex missing")
        .unwrap();

    assert!(missing.reset.starts_with("expected in "));
    assert_eq!(provider.reset, "in 1h 0m");
    assert!(!provider.reset.contains("expected"));
    assert_eq!(unrelated.reset, "unknown");

    let screen = rendered_at(
        &DashboardState {
            windows: views,
            ..DashboardState::default()
        },
        180,
        18,
    );
    assert!(screen.contains("expected in "));
}

#[test]
fn stale_age_and_expired_reset_are_marked_without_claiming_recovery() {
    let rows = vec![
        stored(
            1,
            Provider::Codex,
            WindowKind::Rolling5h,
            90.0,
            Some(1_699_999_999),
            NOW - 1_000,
            Quality::Fresh,
        ),
        stored(
            2,
            Provider::Claude,
            WindowKind::Rolling7d,
            10.0,
            None,
            NOW - 901_000,
            Quality::Fresh,
        ),
    ];
    let views = project_windows(&rows, &AppConfig::default(), NOW);
    assert!(views.iter().all(|view| view.stale));
    assert_eq!(views[0].reset, "expired 1s ago");
    assert!(!views[0].reset.contains("1699999999"));
    assert!(views.iter().all(|view| view.state.contains("stale")));
    assert!(views.iter().all(|view| view.state.contains("fresh")));

    let screen = rendered(&DashboardState {
        windows: views.clone(),
        error: None,
        ..DashboardState::default()
    });
    assert!(screen.contains("expired 1s ago"));
    assert!(screen.matches("stale").count() >= 2);
    assert!(renders_nonblank_cell_with_color(
        &DashboardState {
            windows: views,
            error: None,
            ..DashboardState::default()
        },
        Color::Yellow
    ));
}

#[test]
fn copilot_staleness_uses_the_configured_polling_cadence() {
    let rows = [stored(
        1,
        Provider::GitHubCopilot,
        WindowKind::Monthly,
        40.0,
        Some(NOW / 1_000 + 3_600),
        NOW - 900_000,
        Quality::Fresh,
    )];
    let default_views = project_windows(&rows, &AppConfig::default(), NOW);
    assert!(default_views[0].stale);
    assert!(default_views[0].state.contains("stale"));
    let stale_screen = rendered(&DashboardState {
        windows: default_views,
        ..DashboardState::default()
    });
    assert!(stale_screen.contains("stale"));

    let configured_views = project_windows(
        &rows,
        &AppConfig {
            copilot_refresh_seconds: 600,
            ..AppConfig::default()
        },
        NOW,
    );
    assert!(!configured_views[0].stale);
    assert!(!configured_views[0].state.contains("stale"));
}

#[test]
fn antigravity_staleness_uses_exact_configured_ingest_threshold() {
    let rows = [
        stored_antigravity(
            1,
            "at_boundary",
            40.0,
            Some(NOW / 1_000 + 3_600),
            NOW - 120_000,
            Availability::Allowed,
            Quality::Fresh,
        ),
        stored_antigravity(
            2,
            "past_boundary",
            40.0,
            Some(NOW / 1_000 + 3_600),
            NOW - 120_001,
            Availability::Allowed,
            Quality::Fresh,
        ),
    ];
    let views = project_windows(
        &rows,
        &AppConfig {
            antigravity_stale_after_seconds: 120,
            ..AppConfig::default()
        },
        NOW,
    );
    let boundary = views
        .iter()
        .find(|view| view.scope == "At boundary")
        .unwrap();
    let past = views
        .iter()
        .find(|view| view.scope == "Past boundary")
        .unwrap();
    assert!(!boundary.stale);
    assert!(!boundary.state.contains("stale"));
    assert!(past.stale);
    assert!(past.state.contains("stale"));
}

#[test]
fn dashboard_poll_status_reports_providers_independently() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    storage
        .record_provider_success(Provider::Codex, NOW - 5_000)
        .unwrap();
    storage
        .record_provider_failure(Provider::GitHubCopilot, NOW - 2_000, "timeout")
        .unwrap();
    storage
        .insert_snapshots(&[stored(
            1,
            Provider::Claude,
            WindowKind::Rolling5h,
            42.5,
            None,
            NOW - 7_000,
            Quality::Fresh,
        )
        .snapshot])
        .unwrap();

    let state = DashboardState::load(&storage, &AppConfig::default(), NOW, true, true);
    assert!(state.poll_status.contains("Poll: Codex ok 5s ago"));
    assert!(state.poll_status.contains("Ingest: Claude seen 7s ago"));
    assert!(state.poll_status.contains("Copilot timeout ×1"));
    assert!(state.poll_status.find("Codex").unwrap() < state.poll_status.find("Copilot").unwrap());
    assert!(
        state.poll_status.find("Copilot").unwrap() < state.poll_status.find("Ingest:").unwrap()
    );
    assert!(
        state.poll_status.find("Claude").unwrap() < state.poll_status.find("Antigravity").unwrap()
    );
    assert!(state.poll_status.len() < 120);

    let disabled = DashboardState::load(&storage, &AppConfig::default(), NOW, false, true);
    assert!(disabled.poll_status.contains("Codex off"));
    assert!(disabled.poll_status.contains("Copilot timeout ×1"));
}

#[test]
fn configured_provider_health_renders_without_an_empty_quota_panel() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    storage
        .record_provider_failure(Provider::GitHubCopilot, NOW - 2_000, "timeout")
        .unwrap();

    let state = DashboardState::load(&storage, &AppConfig::default(), NOW, false, true);
    assert!(state.windows.is_empty());
    let screen = rendered(&state);
    assert!(screen.contains("Poll: Codex off · Copilot timeout ×1"));
    assert!(screen.contains("No quota snapshots yet"));
    assert!(!screen.contains("┌ GitHub Copilot windows"));
}

#[test]
fn antigravity_push_ingest_health_renders_without_polling_or_placeholder_rows() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    storage
        .record_provider_failure(Provider::GoogleAntigravity, NOW - 2_000, "parse")
        .unwrap();

    let state = DashboardState::load(&storage, &AppConfig::default(), NOW, false, false);
    assert!(state.windows.is_empty());
    assert!(
        state
            .poll_status
            .contains("Ingest: Claude waiting · Antigravity parse ×1")
    );
    assert!(!state.poll_status.contains("Poll: Antigravity"));
    let screen = rendered_at(&state, 220, 14);
    assert!(screen.contains("Antigravity parse ×1"));
    assert!(screen.contains("Claude/Antigravity ingestion"));
    assert!(!screen.contains("┌ Google AI · Antigravity windows"));
}

#[test]
fn antigravity_health_hides_unapproved_error_classes() {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    storage
        .record_provider_failure(
            Provider::GoogleAntigravity,
            NOW - 2_000,
            "sentinel-private-body",
        )
        .unwrap();

    let state = DashboardState::load(&storage, &AppConfig::default(), NOW, false, false);
    assert!(state.poll_status.contains("Antigravity failed ×1"));
    assert!(!state.poll_status.contains("sentinel-private-body"));
}

#[test]
fn empty_and_error_states_are_helpful() {
    let empty = rendered(&DashboardState::default());
    assert!(empty.contains("No quota snapshots yet"));
    assert!(empty.contains("Claude/Antigravity ingestion"));
    let failure = rendered(&DashboardState {
        windows: Vec::new(),
        error: Some("Latest snapshots are unavailable".into()),
        ..DashboardState::default()
    });
    assert!(failure.contains("Latest snapshots are unavailable"));
}

#[test]
fn constrained_terminal_heights_do_not_panic() {
    let views = project_windows(
        &[
            stored(
                1,
                Provider::Codex,
                WindowKind::Rolling5h,
                25.0,
                Some(1_700_003_600),
                NOW - 30_000,
                Quality::Fresh,
            ),
            stored(
                2,
                Provider::Claude,
                WindowKind::Rolling7d,
                42.5,
                None,
                NOW - 60_000,
                Quality::Partial,
            ),
            stored_with_limit(
                3,
                Provider::GitHubCopilot,
                "premium_interactions",
                WindowKind::Monthly,
                LimitKind::Unlimited,
                0.0,
                Some(NOW / 1_000 + 3_600),
                NOW - 60_000,
                Quality::Fresh,
            ),
        ],
        &AppConfig::default(),
        NOW,
    );
    let state = DashboardState {
        windows: views,
        error: None,
        ..DashboardState::default()
    };

    for width in 1..=32 {
        for height in 1..=8 {
            let _ = rendered_at(&state, width, height);
        }
    }
}

#[test]
fn q_and_escape_quit_but_other_keys_do_not() {
    assert!(should_quit(&Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE
    ))));
    assert!(should_quit(&Event::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE
    ))));
    assert!(!should_quit(&Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::NONE
    ))));
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn two_provider_state() -> DashboardState {
    DashboardState {
        windows: project_windows(
            &[
                stored(
                    1,
                    Provider::Codex,
                    WindowKind::Rolling5h,
                    25.0,
                    Some(NOW / 1_000 + 3_600),
                    NOW,
                    Quality::Fresh,
                ),
                stored(
                    2,
                    Provider::Claude,
                    WindowKind::Rolling7d,
                    42.5,
                    None,
                    NOW,
                    Quality::Fresh,
                ),
            ],
            &AppConfig::default(),
            NOW,
        ),
        ..DashboardState::default()
    }
}

#[test]
fn management_mode_reorders_visible_providers_without_reordering_data() {
    let mut state = two_provider_state();
    let original_windows = state.windows.clone();

    assert_eq!(
        handle_event(&mut state, &key(KeyCode::Char('m'))),
        DashboardAction::Continue
    );
    handle_event(&mut state, &key(KeyCode::Down));
    handle_event(&mut state, &key(KeyCode::Char('[')));
    handle_event(&mut state, &key(KeyCode::Char('m')));

    let screen = rendered(&state);
    assert!(screen.find("Claude windows").unwrap() < screen.find("Codex windows").unwrap());
    assert_eq!(state.windows, original_windows);
    assert!(state.preferences_changed());
    assert_eq!(
        &state.provider_preferences().order[..2],
        &["claude".to_owned(), "codex".to_owned()]
    );
}

#[test]
fn management_mode_hides_selected_provider_without_removing_data() {
    let mut state = two_provider_state();
    let original_windows = state.windows.clone();

    handle_event(&mut state, &key(KeyCode::Char('m')));
    handle_event(&mut state, &key(KeyCode::Down));
    handle_event(&mut state, &key(KeyCode::Char('v')));
    let manager = rendered(&state);
    assert!(manager.contains("> Claude"));
    assert!(manager.contains("[hidden]"));
    assert!(manager.contains("v hide/restore"));
    handle_event(&mut state, &key(KeyCode::Esc));

    let dashboard = rendered(&state);
    assert!(dashboard.contains("Codex windows"));
    assert!(!dashboard.contains("Claude windows"));
    assert_eq!(state.windows, original_windows);
    assert_eq!(state.provider_preferences().hidden, ["claude"]);
}

#[test]
fn hidden_provider_can_be_restored_in_its_saved_relative_order() {
    let preferences = ProviderPreferences {
        order: vec!["claude".into(), "codex".into()],
        hidden: vec!["claude".into()],
    };
    let mut state = two_provider_state();
    state.providers = provider_entries(&preferences);
    let original_windows = state.windows.clone();

    handle_event(&mut state, &key(KeyCode::Char('m')));
    handle_event(&mut state, &key(KeyCode::Char('v')));
    handle_event(&mut state, &key(KeyCode::Esc));

    let screen = rendered(&state);
    assert!(screen.find("Claude windows").unwrap() < screen.find("Codex windows").unwrap());
    assert!(state.provider_preferences().hidden.is_empty());
    assert_eq!(state.windows, original_windows);
}

#[test]
fn all_hidden_dashboard_remains_usable_and_restorable() {
    let mut state = two_provider_state();
    for entry in &mut state.providers {
        entry.visible = false;
    }
    state.error = Some("Latest snapshots are unavailable".into());
    let original_windows = state.windows.clone();

    let hidden = rendered(&state);
    assert!(hidden.contains("No providers are visible"));
    assert!(hidden.contains("Press m to manage and restore providers"));

    handle_event(&mut state, &key(KeyCode::Char('m')));
    let manager = rendered(&state);
    assert!(manager.contains("Manage providers"));
    assert!(manager.contains("> Codex"));
    handle_event(&mut state, &key(KeyCode::Char('v')));
    handle_event(&mut state, &key(KeyCode::Esc));

    let restored = rendered(&state);
    assert!(state.providers[0].visible);
    assert!(
        !state
            .provider_preferences()
            .hidden
            .contains(&"codex".into())
    );
    assert!(restored.contains("Latest snapshots are unavailable"));
    assert_eq!(state.windows, original_windows);
}

#[test]
fn config_persistence_restart_restores_provider_order_and_visibility() {
    let temp = tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let mut state = DashboardState::load(&storage, &AppConfig::default(), NOW, false, false);

    handle_event(&mut state, &key(KeyCode::Char('m')));
    handle_event(&mut state, &key(KeyCode::Down));
    handle_event(&mut state, &key(KeyCode::Char('[')));
    handle_event(&mut state, &key(KeyCode::Char('v')));
    runtime::persist_provider_preferences(&mut state, &config_path).unwrap();
    assert!(!state.preferences_changed());

    let restarted = DashboardState::load(
        &storage,
        &AppConfig::load(&config_path).unwrap(),
        NOW,
        false,
        false,
    );
    assert_eq!(restarted.providers[0].provider, Provider::Claude);
    assert!(!restarted.providers[0].visible);
    assert_eq!(restarted.providers[1].provider, Provider::Codex);
}

#[test]
fn config_persistence_save_failure_is_sanitized_and_retains_dirty_state() {
    let temp = tempdir().unwrap();
    let blocked_parent = temp.path().join("not-a-directory");
    fs::write(&blocked_parent, "blocked").unwrap();
    let config_path = blocked_parent.join("config.toml");
    let mut state = DashboardState::default();
    handle_event(&mut state, &key(KeyCode::Char('m')));
    handle_event(&mut state, &key(KeyCode::Char('v')));

    let error = runtime::persist_provider_preferences(&mut state, &config_path).unwrap_err();

    assert!(state.preferences_changed());
    assert_eq!(error.to_string(), "provider preferences could not be saved");
    assert!(!error.to_string().contains("not-a-directory"));
}
