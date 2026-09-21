use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MIN_CODEX_REFRESH_SECONDS: u64 = 30;
pub const MAX_CODEX_REFRESH_SECONDS: u64 = 15 * 60;
pub const MIN_COPILOT_REFRESH_SECONDS: u64 = 60;
pub const MAX_COPILOT_REFRESH_SECONDS: u64 = 60 * 60;
pub const MIN_CLAUDE_STALE_SECONDS: u64 = 60;
pub const MAX_CLAUDE_STALE_SECONDS: u64 = 24 * 60 * 60;
pub const MIN_ANTIGRAVITY_STALE_SECONDS: u64 = 60;
pub const MAX_ANTIGRAVITY_STALE_SECONDS: u64 = 24 * 60 * 60;
pub const MIN_RETENTION_DAYS: u32 = 1;
pub const MAX_RETENTION_DAYS: u32 = 3_650;
pub const MIN_RESET_SOON_SECONDS: u64 = 60;
pub const MAX_RESET_SOON_SECONDS: u64 = 24 * 60 * 60;

/// Permit-listed local dashboard settings. Provider configuration and credentials
/// intentionally have no representation in this file format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub codex_refresh_seconds: u64,
    #[serde(default = "default_copilot_refresh_seconds")]
    pub copilot_refresh_seconds: u64,
    pub claude_stale_after_seconds: u64,
    #[serde(default = "default_antigravity_stale_after_seconds")]
    pub antigravity_stale_after_seconds: u64,
    pub retention_days: u32,
    pub low_remaining_percent: f64,
    pub reset_soon_seconds: u64,
    pub native_notifications: bool,
    #[serde(default)]
    pub provider_preferences: ProviderPreferences,
}

/// Provider-keyed presentation settings. Keys are reconciled against the
/// providers known by the caller before they are used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPreferences {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub hidden: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledProvider {
    pub key: String,
    pub visible: bool,
}

impl ProviderPreferences {
    /// Keeps configured known keys in their first-listed order, then appends
    /// omitted providers in the caller's default order. Every known provider
    /// remains present even when all of them are hidden.
    pub fn reconcile<'a>(
        &self,
        known_provider_keys: impl IntoIterator<Item = &'a str>,
    ) -> Vec<ReconciledProvider> {
        let mut known = Vec::new();
        for key in known_provider_keys {
            if !known.contains(&key) {
                known.push(key);
            }
        }

        let mut ordered = Vec::with_capacity(known.len());
        for key in &self.order {
            if known.contains(&key.as_str()) && !ordered.contains(&key.as_str()) {
                ordered.push(key.as_str());
            }
        }
        for key in known {
            if !ordered.contains(&key) {
                ordered.push(key);
            }
        }

        ordered
            .into_iter()
            .map(|key| ReconciledProvider {
                key: key.to_owned(),
                visible: !self.hidden.iter().any(|hidden| hidden == key),
            })
            .collect()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            codex_refresh_seconds: 60,
            copilot_refresh_seconds: default_copilot_refresh_seconds(),
            claude_stale_after_seconds: 15 * 60,
            antigravity_stale_after_seconds: default_antigravity_stale_after_seconds(),
            retention_days: 30,
            low_remaining_percent: 20.0,
            reset_soon_seconds: 10 * 60,
            native_notifications: false,
            provider_preferences: ProviderPreferences::default(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_range(
            "codex_refresh_seconds",
            self.codex_refresh_seconds,
            MIN_CODEX_REFRESH_SECONDS,
            MAX_CODEX_REFRESH_SECONDS,
        )?;
        validate_range(
            "copilot_refresh_seconds",
            self.copilot_refresh_seconds,
            MIN_COPILOT_REFRESH_SECONDS,
            MAX_COPILOT_REFRESH_SECONDS,
        )?;
        validate_range(
            "claude_stale_after_seconds",
            self.claude_stale_after_seconds,
            MIN_CLAUDE_STALE_SECONDS,
            MAX_CLAUDE_STALE_SECONDS,
        )?;
        validate_range(
            "antigravity_stale_after_seconds",
            self.antigravity_stale_after_seconds,
            MIN_ANTIGRAVITY_STALE_SECONDS,
            MAX_ANTIGRAVITY_STALE_SECONDS,
        )?;
        validate_range(
            "retention_days",
            u64::from(self.retention_days),
            u64::from(MIN_RETENTION_DAYS),
            u64::from(MAX_RETENTION_DAYS),
        )?;
        if !self.low_remaining_percent.is_finite()
            || !(0.0..=100.0).contains(&self.low_remaining_percent)
        {
            return Err(ConfigError::InvalidPercentage {
                field: "low_remaining_percent",
            });
        }
        validate_range(
            "reset_soon_seconds",
            self.reset_soon_seconds,
            MIN_RESET_SOON_SECONDS,
            MAX_RESET_SOON_SECONDS,
        )
    }

    pub fn load(path: &Path) -> Result<Self, ConfigFileError> {
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let config = Self::default();
                config.validate()?;
                return Ok(config);
            }
            Err(_) => return Err(ConfigFileError::Read(path.to_path_buf())),
        };
        let config = toml::from_str::<Self>(&source)
            .map_err(|_| ConfigFileError::Malformed(path.to_path_buf()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigFileError> {
        self.validate()?;
        let encoded = self.to_toml()?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| ConfigFileError::Write(path.to_path_buf()))?;
        }
        fs::write(path, encoded).map_err(|_| ConfigFileError::Write(path.to_path_buf()))
    }

    pub fn persist_provider_preferences(
        path: &Path,
        provider_preferences: ProviderPreferences,
    ) -> Result<(), ConfigFileError> {
        let mut durable = Self::load(path)?;
        durable.provider_preferences = provider_preferences;
        durable.validate()?;
        durable.save(path)
    }

    pub fn to_toml(&self) -> Result<String, ConfigFileError> {
        toml::to_string_pretty(self).map_err(|_| ConfigFileError::Serialize)
    }
}

const fn default_copilot_refresh_seconds() -> u64 {
    300
}

const fn default_antigravity_stale_after_seconds() -> u64 {
    15 * 60
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    config_file: PathBuf,
}

impl AppPaths {
    pub fn resolve(data_dir_override: Option<&Path>) -> Result<Self, PathError> {
        Self::resolve_with_config(data_dir_override, None)
    }

    /// Resolves standard per-user paths with explicit overrides for portable use.
    pub fn resolve_with_config(
        data_dir_override: Option<&Path>,
        config_file_override: Option<&Path>,
    ) -> Result<Self, PathError> {
        let project = ProjectDirs::from("dev", "agent-tools", "agent-usage-dashboard")
            .ok_or(PathError::Unavailable)?;
        let default_config_dir = project.config_dir().to_path_buf();
        let config_file = config_file_override
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_config_dir.join("config.toml"));
        let config_dir = config_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Ok(Self {
            config_dir,
            data_dir: data_dir_override
                .map(Path::to_path_buf)
                .unwrap_or_else(|| project.data_local_dir().to_path_buf()),
            config_file,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("usage.sqlite3")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_file.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    #[error("{field} must be between {min} and {max}, got {actual}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        actual: u64,
    },
    #[error("{field} must be a finite percentage between 0 and 100")]
    InvalidPercentage { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigFileError {
    #[error("unable to read dashboard config at {0}")]
    Read(PathBuf),
    #[error("dashboard config at {0} is malformed or contains unknown settings")]
    Malformed(PathBuf),
    #[error("unable to write dashboard config at {0}")]
    Write(PathBuf),
    #[error("unable to encode dashboard config")]
    Serialize,
    #[error(transparent)]
    Invalid(#[from] ConfigError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("the operating system did not provide application directories")]
    Unavailable,
}

fn validate_range(field: &'static str, actual: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if !(min..=max).contains(&actual) {
        return Err(ConfigError::OutOfRange {
            field,
            min,
            max,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_persistence_tests.rs"]
mod config_persistence_tests;
