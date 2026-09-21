use std::{fmt, str::FromStr};

use thiserror::Error;
use uuid::Uuid;

const MAX_SCOPE_KEY_LEN: usize = 128;
const MAX_SOURCE_VERSION_LEN: usize = 128;
const MAX_USED_PERCENT: f64 = 1_000_000.0;

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseEnumError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant)),+,
                    _ => Err(ParseEnumError {
                        enum_name: stringify!($name),
                        value: value.to_owned(),
                    }),
                }
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Codex,
    Claude,
    GitHubCopilot,
    GoogleAntigravity,
}

impl Provider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::GitHubCopilot => "github_copilot",
            Self::GoogleAntigravity => "google_antigravity",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::GitHubCopilot => "GitHub Copilot",
            Self::GoogleAntigravity => "Google AI · Antigravity",
        })
    }
}

impl FromStr for Provider {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "github_copilot" => Ok(Self::GitHubCopilot),
            "google_antigravity" => Ok(Self::GoogleAntigravity),
            _ => Err(ParseEnumError {
                enum_name: "Provider",
                value: value.to_owned(),
            }),
        }
    }
}

text_enum!(WindowKind {
    Rolling5h => "rolling_5h",
    Rolling7d => "rolling_7d",
    Spend => "spend",
    Monthly => "monthly",
    Other => "other",
});

text_enum!(LimitKind {
    Limited => "limited",
    Unlimited => "unlimited",
});

text_enum!(Availability {
    Allowed => "allowed",
    Blocked => "blocked",
    Unknown => "unknown",
});

text_enum!(Quality {
    Fresh => "fresh",
    Partial => "partial",
});

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown {enum_name} value: {value}")]
pub struct ParseEnumError {
    pub enum_name: &'static str,
    pub value: String,
}

/// Permit-listed input accepted from provider adapters.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshotInput {
    pub provider: Provider,
    pub scope_key: String,
    pub window_kind: WindowKind,
    pub limit_kind: LimitKind,
    pub window_duration_seconds: Option<i64>,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    pub observed_at: i64,
    pub availability: Availability,
    pub source_version: Option<String>,
    pub quality: Quality,
    pub source_sequence: Uuid,
}

/// A validated normalized row. Remaining percentage is intentionally derived.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    input: QuotaSnapshotInput,
}

impl QuotaSnapshot {
    pub fn new(input: QuotaSnapshotInput) -> Result<Self, ValidationError> {
        validate_text("scope_key", &input.scope_key, MAX_SCOPE_KEY_LEN, false)?;

        if let Some(duration) = input.window_duration_seconds
            && duration <= 0
        {
            return Err(ValidationError::NonPositiveWindowDuration(duration));
        }
        if !input.used_percent.is_finite() {
            return Err(ValidationError::NonFiniteUsedPercent);
        }
        if input.used_percent < 0.0 {
            return Err(ValidationError::NegativeUsedPercent(input.used_percent));
        }
        if input.used_percent > MAX_USED_PERCENT {
            return Err(ValidationError::UsedPercentTooLarge(input.used_percent));
        }
        if input.window_kind != WindowKind::Spend && input.used_percent > 100.0 {
            return Err(ValidationError::SubscriptionPercentAboveHundred(
                input.used_percent,
            ));
        }
        if let Some(resets_at) = input.resets_at
            && resets_at < 0
        {
            return Err(ValidationError::NegativeTimestamp {
                field: "resets_at",
                value: resets_at,
            });
        }
        if input.observed_at < 0 {
            return Err(ValidationError::NegativeTimestamp {
                field: "observed_at",
                value: input.observed_at,
            });
        }
        if let Some(version) = &input.source_version {
            validate_text("source_version", version, MAX_SOURCE_VERSION_LEN, true)?;
        }

        Ok(Self { input })
    }

    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.input.used_percent).clamp(0.0, 100.0)
    }

    pub fn as_input(&self) -> &QuotaSnapshotInput {
        &self.input
    }

    pub fn into_input(self) -> QuotaSnapshotInput {
        self.input
    }
}

impl TryFrom<QuotaSnapshotInput> for QuotaSnapshot {
    type Error = ValidationError;

    fn try_from(input: QuotaSnapshotInput) -> Result<Self, Self::Error> {
        Self::new(input)
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ValidationError {
    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },
    #[error("{field} exceeds its maximum length of {max}")]
    TextTooLong { field: &'static str, max: usize },
    #[error("{field} contains control characters")]
    ControlCharacter { field: &'static str },
    #[error("window duration must be positive, got {0}")]
    NonPositiveWindowDuration(i64),
    #[error("used percentage must be finite")]
    NonFiniteUsedPercent,
    #[error("used percentage must be nonnegative, got {0}")]
    NegativeUsedPercent(f64),
    #[error("used percentage exceeds the accepted magnitude, got {0}")]
    UsedPercentTooLarge(f64),
    #[error("subscription used percentage must not exceed 100, got {0}")]
    SubscriptionPercentAboveHundred(f64),
    #[error("{field} must be a nonnegative Unix timestamp, got {value}")]
    NegativeTimestamp { field: &'static str, value: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RejectedSnapshot {
    pub index: usize,
    pub error: ValidationError,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct NormalizedBatch {
    pub accepted: Vec<QuotaSnapshot>,
    pub rejected: Vec<RejectedSnapshot>,
}

/// Validates rows independently so malformed siblings do not discard good data.
pub fn normalize_rows(rows: impl IntoIterator<Item = QuotaSnapshotInput>) -> NormalizedBatch {
    let mut batch = NormalizedBatch::default();
    for (index, row) in rows.into_iter().enumerate() {
        match QuotaSnapshot::new(row) {
            Ok(snapshot) => batch.accepted.push(snapshot),
            Err(error) => batch.rejected.push(RejectedSnapshot { index, error }),
        }
    }
    batch
}

fn validate_text(
    field: &'static str,
    value: &str,
    max: usize,
    allow_empty: bool,
) -> Result<(), ValidationError> {
    if !allow_empty && value.trim().is_empty() {
        return Err(ValidationError::EmptyText { field });
    }
    if value.len() > max {
        return Err(ValidationError::TextTooLong { field, max });
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::ControlCharacter { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: WindowKind, used_percent: f64) -> QuotaSnapshotInput {
        QuotaSnapshotInput {
            provider: Provider::Codex,
            scope_key: "subscription".into(),
            window_kind: kind,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: Some(18_000),
            used_percent,
            resets_at: Some(1_800_000_000),
            observed_at: 1_700_000_000_000,
            availability: Availability::Unknown,
            source_version: Some("0.1.0".into()),
            quality: Quality::Fresh,
            source_sequence: Uuid::new_v4(),
        }
    }

    #[test]
    fn remaining_is_derived_and_clamped() {
        let ordinary = QuotaSnapshot::new(input(WindowKind::Rolling5h, 42.25)).unwrap();
        let overspent = QuotaSnapshot::new(input(WindowKind::Spend, 125.0)).unwrap();
        assert_eq!(ordinary.remaining_percent(), 57.75);
        assert_eq!(overspent.remaining_percent(), 0.0);
    }

    #[test]
    fn subscription_validation_rejects_invalid_numbers() {
        assert!(matches!(
            QuotaSnapshot::new(input(WindowKind::Rolling7d, 100.01)),
            Err(ValidationError::SubscriptionPercentAboveHundred(_))
        ));
        assert!(matches!(
            QuotaSnapshot::new(input(WindowKind::Rolling7d, f64::NAN)),
            Err(ValidationError::NonFiniteUsedPercent)
        ));
        assert!(matches!(
            QuotaSnapshot::new(input(WindowKind::Rolling7d, -0.1)),
            Err(ValidationError::NegativeUsedPercent(_))
        ));
    }

    #[test]
    fn validates_timestamps_duration_and_safe_text() {
        let mut row = input(WindowKind::Other, 2.0);
        row.window_duration_seconds = Some(0);
        assert!(matches!(
            QuotaSnapshot::new(row),
            Err(ValidationError::NonPositiveWindowDuration(0))
        ));

        let mut row = input(WindowKind::Other, 2.0);
        row.scope_key = "unsafe\nkey".into();
        assert!(matches!(
            QuotaSnapshot::new(row),
            Err(ValidationError::ControlCharacter { .. })
        ));
    }

    #[test]
    fn normalization_keeps_valid_siblings() {
        let good = input(WindowKind::Rolling5h, 20.0);
        let bad = input(WindowKind::Rolling7d, 101.0);
        let batch = normalize_rows([bad, good]);
        assert_eq!(batch.accepted.len(), 1);
        assert_eq!(batch.rejected.len(), 1);
        assert_eq!(batch.rejected[0].index, 0);
    }

    #[test]
    fn enums_round_trip_through_storage_names() {
        assert_eq!("claude".parse::<Provider>().unwrap(), Provider::Claude);
        assert_eq!(
            "github_copilot".parse::<Provider>().unwrap(),
            Provider::GitHubCopilot
        );
        assert_eq!(Provider::GitHubCopilot.as_str(), "github_copilot");
        assert_eq!(Provider::GitHubCopilot.to_string(), "GitHub Copilot");
        assert_eq!(
            "google_antigravity".parse::<Provider>().unwrap(),
            Provider::GoogleAntigravity
        );
        assert_eq!(Provider::GoogleAntigravity.as_str(), "google_antigravity");
        assert_eq!(
            Provider::GoogleAntigravity.to_string(),
            "Google AI · Antigravity"
        );
        assert_eq!(
            "rolling_5h".parse::<WindowKind>().unwrap(),
            WindowKind::Rolling5h
        );
        assert_eq!(
            "monthly".parse::<WindowKind>().unwrap(),
            WindowKind::Monthly
        );
        assert_eq!(WindowKind::Monthly.to_string(), "monthly");
        assert_eq!("limited".parse::<LimitKind>().unwrap(), LimitKind::Limited);
        assert_eq!(
            "unlimited".parse::<LimitKind>().unwrap(),
            LimitKind::Unlimited
        );
        assert_eq!(LimitKind::Unlimited.to_string(), "unlimited");
        assert!("primary".parse::<WindowKind>().is_err());
        assert!("copilot".parse::<Provider>().is_err());
        assert!("gemini".parse::<Provider>().is_err());
        assert!("infinite".parse::<LimitKind>().is_err());
    }
}
