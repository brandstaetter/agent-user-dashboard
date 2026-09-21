use std::fs;

use tempfile::tempdir;

use super::*;

#[test]
fn config_persistence_missing_file_uses_validated_defaults_without_writing() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("nested/config.toml");
    let loaded = AppConfig::load(&path).unwrap();
    assert_eq!(loaded, AppConfig::default());
    assert_eq!(loaded.codex_refresh_seconds, 60);
    assert_eq!(loaded.copilot_refresh_seconds, 300);
    assert_eq!(loaded.claude_stale_after_seconds, 900);
    assert_eq!(loaded.antigravity_stale_after_seconds, 900);
    assert_eq!(loaded.retention_days, 30);
    assert!(!loaded.native_notifications);
    assert!(!path.exists());
}

#[test]
fn config_persistence_round_trip_contains_only_dashboard_settings() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("nested/config.toml");
    let expected = AppConfig {
        codex_refresh_seconds: 90,
        copilot_refresh_seconds: 600,
        claude_stale_after_seconds: 1_200,
        antigravity_stale_after_seconds: 1_800,
        retention_days: 45,
        low_remaining_percent: 12.5,
        reset_soon_seconds: 300,
        native_notifications: true,
        provider_preferences: ProviderPreferences {
            order: vec!["claude".into(), "codex".into()],
            hidden: vec!["github_copilot".into()],
        },
    };
    expected.save(&path).unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), expected);
    let source = fs::read_to_string(path).unwrap();
    for forbidden in [
        "token",
        "oauth",
        "credential",
        "history",
        "executable",
        "runtime",
    ] {
        assert!(!source.to_ascii_lowercase().contains(forbidden));
    }
}

#[test]
fn config_persistence_rejects_malformed_unknown_and_invalid_values() {
    let temp = tempdir().unwrap();
    let malformed = temp.path().join("malformed.toml");
    fs::write(&malformed, "codex_refresh_seconds = [").unwrap();
    assert!(matches!(
        AppConfig::load(&malformed),
        Err(ConfigFileError::Malformed(_))
    ));

    let unknown = temp.path().join("unknown.toml");
    let mut source = AppConfig::default().to_toml().unwrap();
    source.push_str("provider_token = 'secret'\n");
    fs::write(&unknown, source).unwrap();
    let error = AppConfig::load(&unknown).unwrap_err();
    assert!(matches!(&error, ConfigFileError::Malformed(_)));
    assert!(!error.to_string().contains("secret"));

    let invalid = AppConfig {
        codex_refresh_seconds: 29,
        ..AppConfig::default()
    };
    assert!(matches!(
        invalid.save(&temp.path().join("invalid.toml")),
        Err(ConfigFileError::Invalid(_))
    ));
    let invalid_path = temp.path().join("invalid-loaded.toml");
    fs::write(&invalid_path, invalid.to_toml().unwrap()).unwrap();
    assert!(matches!(
        AppConfig::load(&invalid_path),
        Err(ConfigFileError::Invalid(_))
    ));

    let invalid_copilot = AppConfig {
        copilot_refresh_seconds: 59,
        ..AppConfig::default()
    };
    assert!(matches!(
        invalid_copilot.save(&temp.path().join("invalid-copilot.toml")),
        Err(ConfigFileError::Invalid(_))
    ));
}

#[test]
fn config_persistence_defaults_copilot_cadence_for_legacy_files() {
    let source = AppConfig::default()
        .to_toml()
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("copilot_refresh_seconds"))
        .collect::<Vec<_>>()
        .join("\n");
    let config: AppConfig = toml::from_str(&source).unwrap();
    assert_eq!(config.copilot_refresh_seconds, 300);
}

#[test]
fn config_persistence_defaults_antigravity_staleness_for_legacy_files() {
    let source = AppConfig::default()
        .to_toml()
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("antigravity_stale_after_seconds"))
        .collect::<Vec<_>>()
        .join("\n");
    let config: AppConfig = toml::from_str(&source).unwrap();
    assert_eq!(config.antigravity_stale_after_seconds, 900);
}

#[test]
fn config_persistence_defaults_provider_preferences_for_legacy_files() {
    let source = AppConfig::default()
        .to_toml()
        .unwrap()
        .lines()
        .take_while(|line| *line != "[provider_preferences]")
        .collect::<Vec<_>>()
        .join("\n");
    let config: AppConfig = toml::from_str(&source).unwrap();
    assert_eq!(config.provider_preferences, ProviderPreferences::default());
    assert_eq!(
        config
            .provider_preferences
            .reconcile(["codex", "claude", "github_copilot"]),
        reconciled(&[("codex", true), ("claude", true), ("github_copilot", true),])
    );
}

#[test]
fn config_persistence_reconciliation_ignores_duplicate_and_unknown_keys() {
    let preferences = ProviderPreferences {
        order: vec![
            "removed_provider".into(),
            "claude".into(),
            "claude".into(),
            "codex".into(),
        ],
        hidden: vec!["removed_provider".into(), "codex".into(), "codex".into()],
    };

    assert_eq!(
        preferences.reconcile(["codex", "claude", "github_copilot"]),
        reconciled(&[("claude", true), ("codex", false), ("github_copilot", true),])
    );
}

#[test]
fn config_persistence_reconciliation_appends_newly_known_providers() {
    let preferences = ProviderPreferences {
        order: vec!["claude".into(), "codex".into()],
        hidden: Vec::new(),
    };

    assert_eq!(
        preferences.reconcile(["codex", "claude", "new_provider"]),
        reconciled(&[("claude", true), ("codex", true), ("new_provider", true),])
    );
}

#[test]
fn config_persistence_reconciliation_retains_all_hidden_providers_for_management() {
    let preferences = ProviderPreferences {
        order: vec!["claude".into(), "codex".into()],
        hidden: vec!["codex".into(), "claude".into()],
    };

    assert_eq!(
        preferences.reconcile(["codex", "claude"]),
        reconciled(&[("claude", false), ("codex", false)])
    );
}

#[test]
fn config_persistence_validates_antigravity_staleness_inclusive_bounds() {
    for accepted in [MIN_ANTIGRAVITY_STALE_SECONDS, MAX_ANTIGRAVITY_STALE_SECONDS] {
        AppConfig {
            antigravity_stale_after_seconds: accepted,
            ..AppConfig::default()
        }
        .validate()
        .unwrap();
    }

    for rejected in [
        MIN_ANTIGRAVITY_STALE_SECONDS - 1,
        MAX_ANTIGRAVITY_STALE_SECONDS + 1,
    ] {
        let error = AppConfig {
            antigravity_stale_after_seconds: rejected,
            ..AppConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::OutOfRange {
                field: "antigravity_stale_after_seconds",
                min: MIN_ANTIGRAVITY_STALE_SECONDS,
                max: MAX_ANTIGRAVITY_STALE_SECONDS,
                actual,
            } if actual == rejected
        ));
    }
}

#[test]
fn config_persistence_path_override_is_exact() {
    let temp = tempdir().unwrap();
    let config_path = temp.path().join("portable/settings.toml");
    let paths = AppPaths::resolve_with_config(Some(temp.path()), Some(&config_path)).unwrap();
    assert_eq!(paths.config_path(), config_path);
    assert_eq!(paths.database_path(), temp.path().join("usage.sqlite3"));
}

fn reconciled(expected: &[(&str, bool)]) -> Vec<ReconciledProvider> {
    expected
        .iter()
        .map(|(key, visible)| ReconciledProvider {
            key: (*key).into(),
            visible: *visible,
        })
        .collect()
}
