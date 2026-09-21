use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use tempfile::tempdir;
use uuid::Uuid;

use super::*;
use crate::domain::{Availability, LimitKind, Provider, Quality, WindowKind};
use crate::providers::codex::CodexObservation;
use crate::providers::github_copilot::{CopilotError, CopilotObservation, CopilotPhase};

#[test]
fn parses_commands_global_data_dir_and_custom_codex_path() {
    let parsed = Cli::try_parse_from([
        "usage",
        "--data-dir",
        "portable",
        "refresh",
        "codex",
        "--executable",
        "tools/codex",
    ])
    .unwrap();
    assert_eq!(parsed.data_dir, Some(PathBuf::from("portable")));
    assert!(matches!(
        parsed.command,
        Some(Command::Refresh {
            provider: RefreshCommand::Codex { executable }
        }) if executable == Path::new("tools/codex")
    ));

    let help = Cli::try_parse_from(["usage", "--help"]).unwrap_err();
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    let ingest = Cli::try_parse_from(["usage", "ingest", "claude"]).unwrap();
    assert!(matches!(
        ingest.command,
        Some(Command::Ingest {
            provider: IngestCommand::Claude
        })
    ));
}

#[test]
fn parses_watch_history_and_validated_alert_settings() {
    let watch = Cli::try_parse_from([
        "usage",
        "--codex-refresh-seconds",
        "30",
        "--low-remaining-percent",
        "12.5",
        "--native-notifications",
        "watch",
        "codex",
    ])
    .unwrap();
    assert!(matches!(
        watch.command,
        Some(Command::Watch {
            provider: WatchCommand::Codex { .. }
        })
    ));
    let config = merged_config(AppConfig::default(), &watch).unwrap();
    assert_eq!(config.codex_refresh_seconds, 30);
    assert_eq!(config.low_remaining_percent, 12.5);
    assert!(config.native_notifications);

    let history = Cli::try_parse_from(["usage", "history", "--limit", "25"]).unwrap();
    assert!(matches!(
        history.command,
        Some(Command::History { limit: 25 })
    ));
    assert!(Cli::try_parse_from(["usage", "history", "--limit", "0"]).is_err());

    let invalid =
        Cli::try_parse_from(["usage", "--codex-refresh-seconds", "29", "dashboard"]).unwrap();
    assert!(matches!(
        merged_config(AppConfig::default(), &invalid),
        Err(CliError::Config(_))
    ));

    let watch_help = Cli::try_parse_from(["usage", "watch", "codex", "--help"])
        .unwrap_err()
        .to_string();
    assert!(watch_help.contains("Continuously poll Codex app-server"));
    assert!(!watch_help.contains("once and persist"));
}

#[test]
fn config_persistence_cli_applies_only_explicit_overrides() {
    let cli = Cli::try_parse_from([
        "usage",
        "--config-file",
        "portable.toml",
        "--codex-refresh-seconds",
        "120",
        "--no-native-notifications",
        "config",
        "show",
    ])
    .unwrap();
    let persisted = AppConfig {
        codex_refresh_seconds: 60,
        copilot_refresh_seconds: 300,
        claude_stale_after_seconds: 2_400,
        antigravity_stale_after_seconds: 3_000,
        retention_days: 90,
        low_remaining_percent: 7.5,
        reset_soon_seconds: 180,
        native_notifications: true,
        provider_preferences: Default::default(),
    };
    let merged = merged_config(persisted.clone(), &cli).unwrap();
    assert_eq!(merged.codex_refresh_seconds, 120);
    assert!(!merged.native_notifications);
    assert_eq!(merged.claude_stale_after_seconds, 2_400);
    assert_eq!(merged.antigravity_stale_after_seconds, 3_000);
    assert_eq!(merged.retention_days, 90);
    assert_eq!(merged.low_remaining_percent, 7.5);
    assert_eq!(merged.reset_soon_seconds, 180);
    assert_eq!(cli.config_file, Some(PathBuf::from("portable.toml")));
    assert!(matches!(
        cli.command,
        Some(Command::Config {
            action: ConfigCommand::Show
        })
    ));

    let no_overrides = Cli::try_parse_from(["usage", "config", "show"]).unwrap();
    assert_eq!(
        merged_config(persisted.clone(), &no_overrides).unwrap(),
        persisted
    );
}

#[test]
fn config_persistence_tui_preferences_do_not_persist_cli_overrides() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("settings.toml");
    let durable = AppConfig {
        codex_refresh_seconds: 60,
        retention_days: 90,
        ..AppConfig::default()
    };
    durable.save(&path).unwrap();

    let cli =
        Cli::try_parse_from(["usage", "--codex-refresh-seconds", "120", "dashboard"]).unwrap();
    let effective = merged_config(AppConfig::load(&path).unwrap(), &cli).unwrap();
    assert_eq!(effective.codex_refresh_seconds, 120);

    let preferences = crate::config::ProviderPreferences {
        order: vec!["claude".into(), "codex".into()],
        hidden: vec!["codex".into()],
    };
    AppConfig::persist_provider_preferences(&path, preferences.clone()).unwrap();

    let reloaded = AppConfig::load(&path).unwrap();
    assert_eq!(reloaded.provider_preferences, preferences);
    assert_eq!(reloaded.codex_refresh_seconds, 60);
    assert_eq!(reloaded.retention_days, 90);
}

#[test]
fn parses_copilot_commands_runtime_polling_and_validated_cadence() {
    let refresh = Cli::try_parse_from([
        "usage",
        "--copilot-refresh-seconds",
        "600",
        "--copilot-runtime",
        "tools/copilot-runtime",
        "--no-copilot-poll",
        "refresh",
        "copilot",
    ])
    .unwrap();
    assert!(matches!(
        refresh.command,
        Some(Command::Refresh {
            provider: RefreshCommand::Copilot
        })
    ));
    assert_eq!(
        refresh.copilot_runtime,
        Some(PathBuf::from("tools/copilot-runtime"))
    );
    assert!(refresh.no_copilot_poll);
    assert_eq!(
        merged_config(AppConfig::default(), &refresh)
            .unwrap()
            .copilot_refresh_seconds,
        600
    );
    let persisted = merged_config(AppConfig::default(), &refresh)
        .unwrap()
        .to_toml()
        .unwrap();
    assert!(!persisted.contains("tools/copilot-runtime"));
    assert!(!persisted.contains("copilot_runtime"));

    let watch = Cli::try_parse_from(["usage", "watch", "copilot"]).unwrap();
    assert!(matches!(
        watch.command,
        Some(Command::Watch {
            provider: WatchCommand::Copilot
        })
    ));
    for invalid in [59, 3_601] {
        let cli = Cli::try_parse_from([
            "usage",
            "--copilot-refresh-seconds",
            &invalid.to_string(),
            "dashboard",
        ])
        .unwrap();
        assert!(matches!(
            merged_config(AppConfig::default(), &cli),
            Err(CliError::Config(_))
        ));
    }
}

#[test]
fn dashboard_polling_cli_has_configurable_executable_and_offline_escape_hatch() {
    let cli = Cli::try_parse_from([
        "usage",
        "--codex-executable",
        "tools/codex-local",
        "--no-codex-poll",
        "dashboard",
    ])
    .unwrap();
    assert_eq!(cli.codex_executable, PathBuf::from("tools/codex-local"));
    assert!(cli.no_codex_poll);
}

#[test]
fn parses_shell_free_claude_relay_trailing_argv() {
    let cli = Cli::try_parse_from([
        "usage",
        "relay",
        "claude",
        "--",
        "npx",
        "-y",
        "ccstatusline@latest",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Relay {
            provider: RelayCommand::Claude { command }
        }) if command == ["npx", "-y", "ccstatusline@latest"]
    ));
}

#[test]
fn parses_antigravity_ingest_relay_and_stale_override_but_not_polling() {
    let ingest = Cli::try_parse_from([
        "usage",
        "--antigravity-stale-after-seconds",
        "1200",
        "ingest",
        "antigravity",
    ])
    .unwrap();
    assert!(matches!(
        ingest.command,
        Some(Command::Ingest {
            provider: IngestCommand::Antigravity
        })
    ));
    assert_eq!(
        merged_config(AppConfig::default(), &ingest)
            .unwrap()
            .antigravity_stale_after_seconds,
        1_200
    );

    let relay = Cli::try_parse_from([
        "usage",
        "relay",
        "antigravity",
        "--",
        "agy-status",
        "--compact",
    ])
    .unwrap();
    assert!(matches!(
        relay.command,
        Some(Command::Relay {
            provider: RelayCommand::Antigravity { command }
        }) if command == ["agy-status", "--compact"]
    ));

    assert!(Cli::try_parse_from(["usage", "refresh", "antigravity"]).is_err());
    assert!(Cli::try_parse_from(["usage", "watch", "antigravity"]).is_err());
    assert!(Cli::try_parse_from(["usage", "ingest", "gemini"]).is_err());
    assert!(Cli::try_parse_from(["usage", "relay", "gemini", "formatter"]).is_err());
    let refresh_help = Cli::try_parse_from(["usage", "refresh", "--help"])
        .unwrap_err()
        .to_string();
    let watch_help = Cli::try_parse_from(["usage", "watch", "--help"])
        .unwrap_err()
        .to_string();
    assert!(!refresh_help.contains("Query Google Antigravity"));
    assert!(!watch_help.contains("poll Google Antigravity"));
}

#[test]
fn antigravity_stale_override_uses_config_bounds() {
    for invalid in [59_u64, 86_401] {
        let cli = Cli::try_parse_from([
            "usage",
            "--antigravity-stale-after-seconds",
            &invalid.to_string(),
            "dashboard",
        ])
        .unwrap();
        assert!(matches!(
            merged_config(AppConfig::default(), &cli),
            Err(CliError::Config(_))
        ));
    }
}

const ANTIGRAVITY_FIXTURE: &[u8] = br#"{
    "product":"antigravity",
    "version":"1.2.3",
    "private":"do-not-print-or-persist",
    "quota":{
        "pro-model":{"remaining_fraction":0.25,"reset_time":"2026-09-21T00:00:00Z"},
        "invalid-secret-bucket":{"remaining_fraction":2,"private":"do-not-print-or-persist"}
    }
}"#;

#[test]
fn direct_antigravity_ingest_persists_partial_batch_summary_and_success_health() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let observed_at = 1_700_000_000_000;
    let mut output = Vec::new();

    let snapshots = ingest_antigravity(
        &storage,
        ANTIGRAVITY_FIXTURE,
        &mut output,
        observed_at,
        &AppConfig::default(),
    )
    .unwrap();

    assert_eq!(snapshots.len(), 1);
    let rows = storage.latest_snapshots().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].snapshot.as_input().provider,
        Provider::GoogleAntigravity
    );
    assert_eq!(rows[0].snapshot.as_input().scope_key, "pro-model");
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Google AI · Antigravity: accepted 1, rejected 1"));
    assert!(!output.contains("pro-model"));
    assert!(!output.contains("invalid-secret-bucket"));
    assert!(!output.contains("do-not-print-or-persist"));
    let database = fs::read(temp.path().join("usage.sqlite3")).unwrap();
    assert!(
        !database
            .windows(b"do-not-print-or-persist".len())
            .any(|window| window == b"do-not-print-or-persist")
    );
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_attempt_at, Some(observed_at));
    assert_eq!(health.last_success_at, Some(observed_at));
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.last_error_class, None);
}

#[test]
fn direct_antigravity_failures_are_sanitized_and_record_approved_classes() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let observed_at = 1_700_000_000_000;
    let sentinel = br#"{"private":"do-not-echo"} trailing"#;

    let parse_error = ingest_antigravity(
        &storage,
        sentinel.as_slice(),
        Vec::new(),
        observed_at,
        &AppConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(
        parse_error,
        CliError::AntigravityIngest { class: "parse" }
    ));
    assert!(!parse_error.to_string().contains("do-not-echo"));
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_error_class.as_deref(), Some("parse"));

    let oversized = vec![b'x'; ANTIGRAVITY_MAX_STATUS_INPUT_BYTES + 1];
    let oversize_error = ingest_antigravity(
        &storage,
        oversized.as_slice(),
        Vec::new(),
        observed_at + 1,
        &AppConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(
        oversize_error,
        CliError::AntigravityIngest { class: "oversize" }
    ));
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_error_class.as_deref(), Some("oversize"));
    assert_eq!(health.consecutive_failures, 2);
}

#[test]
fn empty_antigravity_quota_is_a_successful_zero_window_observation() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let observed_at = 1_700_000_000_000;
    let mut output = Vec::new();

    let rows = ingest_antigravity(
        &storage,
        br#"{"product":"antigravity","quota":null}"#.as_slice(),
        &mut output,
        observed_at,
        &AppConfig::default(),
    )
    .unwrap();

    assert!(rows.is_empty());
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "Google AI · Antigravity: accepted 0, rejected 0\n"
    );
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_success_at, Some(observed_at));
    assert_eq!(health.last_error_class, None);
}

#[test]
fn direct_antigravity_storage_failure_uses_only_the_storage_class() {
    let temp = tempdir().unwrap();
    let database = temp.path().join("usage.sqlite3");
    let storage = Storage::open(&database).unwrap();
    fs::remove_file(&database).unwrap();
    fs::create_dir(&database).unwrap();

    let error = ingest_antigravity(
        &storage,
        ANTIGRAVITY_FIXTURE,
        Vec::new(),
        1_700_000_000_000,
        &AppConfig::default(),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CliError::AntigravityIngest { class: "storage" }
    ));
    assert_eq!(
        error.to_string(),
        "Google Antigravity ingestion failed: storage"
    );
}

const CLI_RELAY_HELPER_SOURCE: &str = r#"
use std::{env, io::{Read, Write}, process};

fn main() {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    std::io::stdout().write_all(&input).unwrap();
    std::io::stderr().write_all(b"cli-relay-stderr").unwrap();
    let exit_code = env::args().nth(1).unwrap().parse().unwrap();
    process::exit(exit_code);
}
"#;

fn cli_relay_helper() -> &'static PathBuf {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER.get_or_init(|| {
        let directory = tempdir().unwrap().keep();
        let source = directory.join("cli_relay_helper.rs");
        let executable = directory.join(format!("cli-relay-helper{}", env::consts::EXE_SUFFIX));
        fs::write(&source, CLI_RELAY_HELPER_SOURCE).unwrap();
        let compiler = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = ProcessCommand::new(compiler)
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "CLI relay helper compilation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        executable
    })
}

#[test]
fn antigravity_relay_preserves_streams_and_records_success() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let config = AppConfig::default();
    let observed_at = 1_700_000_000_000;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let command = [cli_relay_helper().as_os_str().to_owned(), "0".into()];

    let outcome = relay_antigravity(
        Some((&storage, &config)),
        ANTIGRAVITY_FIXTURE,
        &mut stdout,
        &mut stderr,
        &command,
        observed_at,
    )
    .unwrap();

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(stdout, ANTIGRAVITY_FIXTURE);
    assert_eq!(stderr, b"cli-relay-stderr");
    assert_eq!(storage.latest_snapshots().unwrap().len(), 1);
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_success_at, Some(observed_at));
    assert_eq!(health.last_error_class, None);
}

fn relay_cli(data_dir: &Path, config_file: &Path, stale_override: Option<u64>) -> Cli {
    let mut args = vec![
        OsString::from("usage"),
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_owned(),
        OsString::from("--config-file"),
        config_file.as_os_str().to_owned(),
    ];
    if let Some(value) = stale_override {
        args.push(OsString::from("--antigravity-stale-after-seconds"));
        args.push(OsString::from(value.to_string()));
    }
    args.extend([
        OsString::from("relay"),
        OsString::from("antigravity"),
        OsString::from("--"),
        cli_relay_helper().as_os_str().to_owned(),
        OsString::from("0"),
    ]);
    Cli::try_parse_from(args).unwrap()
}

#[test]
fn relay_context_applies_valid_antigravity_stale_override() {
    let temp = tempdir().unwrap();
    let config_file = temp.path().join("settings.toml");
    let cli = relay_cli(temp.path(), &config_file, Some(1_200));

    let (_, config) = relay_context(&cli).expect("valid relay context");

    assert_eq!(config.antigravity_stale_after_seconds, 1_200);
}

#[test]
fn invalid_relay_override_disables_ingestion_without_changing_child_authority() {
    let temp = tempdir().unwrap();
    let cli = relay_cli(temp.path(), &temp.path().join("settings.toml"), Some(59));
    assert!(relay_context(&cli).is_none());

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let command = [cli_relay_helper().as_os_str().to_owned(), "23".into()];
    let outcome = relay_antigravity(
        None,
        ANTIGRAVITY_FIXTURE,
        &mut stdout,
        &mut stderr,
        &command,
        1_700_000_000_000,
    )
    .unwrap();

    assert_eq!(outcome.exit_code, 23);
    assert_eq!(stdout, ANTIGRAVITY_FIXTURE);
    assert_eq!(stderr, b"cli-relay-stderr");
    assert!(!temp.path().join("usage.sqlite3").exists());
}

#[test]
fn invalid_persisted_relay_config_disables_only_local_ingestion() {
    let temp = tempdir().unwrap();
    let config_file = temp.path().join("settings.toml");
    fs::write(&config_file, "antigravity_stale_after_seconds = 59\n").unwrap();
    let cli = relay_cli(temp.path(), &config_file, None);

    assert!(relay_context(&cli).is_none());

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let command = [cli_relay_helper().as_os_str().to_owned(), "0".into()];
    let outcome = relay_antigravity(
        None,
        ANTIGRAVITY_FIXTURE,
        &mut stdout,
        &mut stderr,
        &command,
        1_700_000_000_000,
    )
    .unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(stdout, ANTIGRAVITY_FIXTURE);
    assert_eq!(stderr, b"cli-relay-stderr");
    assert!(!temp.path().join("usage.sqlite3").exists());
}

#[test]
fn antigravity_relay_child_failure_is_authoritative_and_health_is_wrapped_command() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let config = AppConfig::default();
    let observed_at = 1_700_000_000_000;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let command = [cli_relay_helper().as_os_str().to_owned(), "23".into()];

    let outcome = relay_antigravity(
        Some((&storage, &config)),
        ANTIGRAVITY_FIXTURE,
        &mut stdout,
        &mut stderr,
        &command,
        observed_at,
    )
    .unwrap();

    assert_eq!(outcome.exit_code, 23);
    assert_eq!(stdout, ANTIGRAVITY_FIXTURE);
    assert_eq!(stderr, b"cli-relay-stderr");
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_error_class.as_deref(), Some("wrapped_command"));
    assert_eq!(health.consecutive_failures, 1);
}

#[test]
fn antigravity_relay_parse_failure_is_silent_and_durable() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let config = AppConfig::default();
    let observed_at = 1_700_000_000_000;
    let input = br#"{"private":"relay-do-not-echo"} trailing"#;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let command = [cli_relay_helper().as_os_str().to_owned(), "0".into()];

    let outcome = relay_antigravity(
        Some((&storage, &config)),
        input.as_slice(),
        &mut stdout,
        &mut stderr,
        &command,
        observed_at,
    )
    .unwrap();

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(stdout, input);
    assert_eq!(stderr, b"cli-relay-stderr");
    assert!(storage.latest_snapshots().unwrap().is_empty());
    let health = storage
        .provider_state(Provider::GoogleAntigravity)
        .unwrap()
        .unwrap();
    assert_eq!(health.last_error_class.as_deref(), Some("parse"));
    assert_eq!(health.consecutive_failures, 1);
}

#[test]
fn relayed_claude_capture_persists_best_effort_without_summary_output() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let input = br#"{"private":"discard","rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1800000000}}}"#;

    ingest_relayed_claude(
        Some((&storage, &AppConfig::default())),
        CapturedInput::Complete(input),
        1_700_000_000_000,
    )
    .unwrap();

    let rows = storage.latest_snapshots().unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows[0].snapshot.as_input();
    assert_eq!(row.provider, Provider::Claude);
    assert_eq!(row.used_percent, 42.5);
    assert!(!format!("{row:?}").contains("discard"));
    assert!(storage.recent_alerts(10).unwrap().is_empty());
}

#[test]
fn relayed_claude_malformed_capture_is_a_sanitized_callback_failure() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();

    let result = ingest_relayed_claude(
        Some((&storage, &AppConfig::default())),
        CapturedInput::Complete(br#"{"private":"do-not-echo"} trailing"#),
        1_700_000_000_000,
    );

    assert_eq!(result, Err(()));
    assert!(storage.latest_snapshots().unwrap().is_empty());
}

#[test]
fn relayed_claude_overflow_and_absent_context_skip_ingestion() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let config = AppConfig::default();

    ingest_relayed_claude(
        Some((&storage, &config)),
        CapturedInput::Overflow,
        1_700_000_000_000,
    )
    .unwrap();
    ingest_relayed_claude(
        None,
        CapturedInput::Complete(b"not-json-and-no-local-context"),
        1_700_000_000_000,
    )
    .unwrap();

    assert!(storage.latest_snapshots().unwrap().is_empty());
}

#[test]
fn data_directory_override_resolves_the_sqlite_path() {
    assert_eq!(
        database_path(Some(Path::new("portable"))).unwrap(),
        PathBuf::from("portable").join("usage.sqlite3")
    );
}

#[test]
fn claude_ingest_round_trips_and_summary_excludes_unrelated_input() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let input = br#"{"session_id":"do-not-print","transcript_path":"private/path","rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1800000000},"seven_day":{"used_percentage":17}}}"#;
    let mut output = Vec::new();

    ingest_claude(&storage, input.as_slice(), &mut output, 1_700_000_000_000).unwrap();

    let rows = storage.latest_snapshots().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].snapshot.as_input().provider, Provider::Claude);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Claude: 5h 42.5% used, 57.5% left"));
    assert!(!output.contains("do-not-print"));
    assert!(!output.contains("private/path"));
    assert!(output.len() < 120);
}

struct FakeProbe;

impl CodexProbe for FakeProbe {
    fn read(&self, observed_at: i64) -> Result<CodexObservation, CodexError> {
        Ok(CodexObservation {
            snapshots: vec![QuotaSnapshotInput {
                provider: Provider::Codex,
                scope_key: "subscription".into(),
                window_kind: WindowKind::Rolling5h,
                limit_kind: LimitKind::Limited,
                window_duration_seconds: Some(18_000),
                used_percent: 25.0,
                resets_at: Some(1_800_000_000),
                observed_at,
                availability: Availability::Allowed,
                source_version: Some("fake".into()),
                quality: Quality::Fresh,
                source_sequence: Uuid::new_v4(),
            }],
            rejected_windows: 0,
        })
    }
}

struct FakeCopilotProbe {
    results: Mutex<VecDeque<Result<CopilotObservation, CopilotError>>>,
}

impl FakeCopilotProbe {
    fn new(results: impl IntoIterator<Item = Result<CopilotObservation, CopilotError>>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl CopilotProbe for FakeCopilotProbe {
    fn read(&self, _observed_at: i64) -> Result<CopilotObservation, CopilotError> {
        self.results.lock().unwrap().pop_front().unwrap()
    }
}

fn copilot_row(scope_key: &str, limit_kind: LimitKind, used_percent: f64) -> QuotaSnapshotInput {
    QuotaSnapshotInput {
        provider: Provider::GitHubCopilot,
        scope_key: scope_key.into(),
        window_kind: WindowKind::Monthly,
        limit_kind,
        window_duration_seconds: None,
        used_percent,
        resets_at: Some(1_800_000_000),
        observed_at: 1_700_000_000_000,
        availability: Availability::Allowed,
        source_version: Some("fake".into()),
        quality: Quality::Fresh,
        source_sequence: Uuid::new_v4(),
    }
}

#[test]
fn injected_copilot_refresh_persists_finite_and_unlimited_rows_and_health() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let probe = FakeCopilotProbe::new([Ok(CopilotObservation {
        snapshots: vec![
            copilot_row("premium_interactions", LimitKind::Limited, 25.0),
            copilot_row("chat", LimitKind::Unlimited, 0.0),
        ],
        rejected_windows: 0,
    })]);
    let mut output = Vec::new();

    let rows = refresh_copilot(
        &storage,
        &probe,
        &mut output,
        1_700_000_000_000,
        &AppConfig::default(),
        false,
    )
    .unwrap();

    assert_eq!(rows.len(), 2);
    let stored = storage.latest_snapshots().unwrap();
    assert!(stored.iter().any(|row| {
        let row = row.snapshot.as_input();
        row.scope_key == "premium_interactions" && row.limit_kind == LimitKind::Limited
    }));
    assert!(stored.iter().any(|row| {
        let row = row.snapshot.as_input();
        row.scope_key == "chat" && row.limit_kind == LimitKind::Unlimited
    }));
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "GitHub Copilot: Premium interactions 25.0% used, 75.0% left | Chat Unlimited\n"
    );
    let state = storage
        .provider_state(Provider::GitHubCopilot)
        .unwrap()
        .unwrap();
    assert_eq!(state.last_success_at, Some(1_700_000_000_000));
    assert_eq!(state.consecutive_failures, 0);
}

#[test]
fn copilot_empty_success_prints_exact_summary_and_records_success() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let probe = FakeCopilotProbe::new([Ok(CopilotObservation {
        snapshots: Vec::new(),
        rejected_windows: 0,
    })]);
    let mut output = Vec::new();

    refresh_copilot(
        &storage,
        &probe,
        &mut output,
        1_700_000_000_000,
        &AppConfig::default(),
        false,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "GitHub Copilot: no quota windows\n"
    );
    assert!(storage.latest_snapshots().unwrap().is_empty());
    assert_eq!(
        storage
            .provider_state(Provider::GitHubCopilot)
            .unwrap()
            .unwrap()
            .last_success_at,
        Some(1_700_000_000_000)
    );
}

#[test]
fn copilot_failures_store_only_stable_classes_and_leave_other_health_untouched() {
    let errors = [
        CopilotError::Spawn,
        CopilotError::Timeout(CopilotPhase::Rpc),
        CopilotError::NotAuthenticated,
        CopilotError::NotEntitled,
        CopilotError::Protocol,
        CopilotError::Rpc,
    ];
    for error in errors {
        let temp = tempdir().unwrap();
        let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
        storage
            .record_provider_success(Provider::Codex, 1_699_999_999_000)
            .unwrap();
        let codex_before = storage.provider_state(Provider::Codex).unwrap();
        let probe = FakeCopilotProbe::new([Err(error)]);
        let mut output = Vec::new();
        let result = refresh_copilot(
            &storage,
            &probe,
            &mut output,
            1_700_000_000_000,
            &AppConfig::default(),
            false,
        );

        assert!(matches!(
            &result,
            Err(CliError::CopilotRefresh { class }) if *class == error.class()
        ));
        assert!(output.is_empty());
        let state = storage
            .provider_state(Provider::GitHubCopilot)
            .unwrap()
            .unwrap();
        assert_eq!(state.last_error_class.as_deref(), Some(error.class()));
        assert_eq!(
            storage.provider_state(Provider::Codex).unwrap(),
            codex_before
        );
        let message = result.unwrap_err().to_string();
        assert_eq!(
            message,
            format!("GitHub Copilot refresh failed: {}", error.class())
        );
        assert!(!message.contains("private"));
        assert!(!message.contains("runtime"));
    }
}

#[test]
fn copilot_watch_uses_configured_cadence_and_stops_cleanly_with_fake_probe() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let probe = FakeCopilotProbe::new([Ok(CopilotObservation {
        snapshots: Vec::new(),
        rejected_windows: 0,
    })]);
    let config = AppConfig {
        copilot_refresh_seconds: 600,
        ..AppConfig::default()
    };
    let running = AtomicBool::new(true);
    let mut waited = None;
    let mut output = Vec::new();

    watch::run_copilot_loop(
        &storage,
        &probe,
        &config,
        &running,
        &mut output,
        || Ok(1_700_000_000_000),
        |signal, duration| {
            waited = Some(duration);
            signal.store(false, Ordering::SeqCst);
        },
        false,
    )
    .unwrap();

    assert_eq!(waited, Some(Duration::from_secs(600)));
    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("Watching GitHub Copilot every 600 seconds"));
    assert!(output.contains("GitHub Copilot: no quota windows"));
    assert!(output.ends_with("GitHub Copilot watch stopped.\n"));
    assert!(!running.load(Ordering::SeqCst));
}

#[test]
fn injected_codex_probe_persists_without_an_authenticated_account() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let mut output = Vec::new();

    refresh_codex(&storage, &FakeProbe, &mut output, 1_700_000_000_000).unwrap();

    assert_eq!(storage.latest_snapshots().unwrap().len(), 1);
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "Codex: 5h 25.0% used, 75.0% left\n"
    );
}

#[test]
fn ingest_rejects_more_than_one_json_value() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let error = ingest_claude(
        &storage,
        br#"{"rate_limits":{}} {"second":true}"#.as_slice(),
        Vec::new(),
        1_700_000_000_000,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        CliError::Claude(ClaudeParseError::InvalidDocument)
    ));
}

#[test]
fn successful_claude_ingest_evaluates_alerts_and_history_without_raw_input() {
    let temp = tempdir().unwrap();
    let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
    let reset = 1_800_000_000;
    let prior = QuotaSnapshot::new(QuotaSnapshotInput {
        provider: Provider::Claude,
        scope_key: "subscription".into(),
        window_kind: WindowKind::Rolling5h,
        limit_kind: LimitKind::Limited,
        window_duration_seconds: Some(18_000),
        used_percent: 70.0,
        resets_at: Some(reset),
        observed_at: 1_700_000_000_000 - 1,
        availability: Availability::Unknown,
        source_version: None,
        quality: Quality::Fresh,
        source_sequence: Uuid::new_v4(),
    })
    .unwrap();
    storage.insert_snapshots(&[prior]).unwrap();
    let input = format!(
        r#"{{"private":"discard","rate_limits":{{"five_hour":{{"used_percentage":80,"resets_at":{reset}}}}}}}"#
    );
    let mut output = Vec::new();
    let now = 1_700_000_000_000;
    let rows = ingest_claude(&storage, input.as_bytes(), &mut output, now).unwrap();
    finish_observation(
        &storage,
        &rows,
        &AppConfig::default(),
        now,
        &mut output,
        false,
    )
    .unwrap();
    assert_eq!(storage.recent_alerts(10).unwrap().len(), 1);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("ALERT:"));
    assert!(!output.contains("private"));

    let mut history_output = Vec::new();
    history::write(&storage, &mut history_output, 10, now).unwrap();
    let history_output = String::from_utf8(history_output).unwrap();
    assert!(history_output.contains("kind=low_remaining"));
    assert!(history_output.contains("observed=0s ago"));
    assert!(history_output.contains("reset=in 1157d 9h"));
    assert!(history_output.contains("fired=0s ago"));
    assert!(!history_output.contains("cycle="));
    assert!(!history_output.contains(&now.to_string()));
    assert!(!history_output.contains(&reset.to_string()));
    assert!(!history_output.contains("discard"));
}
