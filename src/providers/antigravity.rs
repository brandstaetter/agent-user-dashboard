use std::collections::BTreeMap;

use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::domain::{
    Availability, LimitKind, Provider, Quality, QuotaSnapshot, QuotaSnapshotInput, WindowKind,
};

/// Maximum size of one caller-supplied Antigravity status-line document.
pub const MAX_STATUS_INPUT_BYTES: usize = 1024 * 1024;

const MAX_BUCKET_KEY_BYTES: usize = 64;
const MAX_VERSION_BYTES: usize = 64;
const RESET_DISAGREEMENT_TOLERANCE_SECONDS: u64 = 60;

/// Permit-listed normalized result of parsing one Antigravity status document.
///
/// The original document and individual raw quota entries are discarded before
/// this value crosses the provider boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAntigravityStatus {
    pub snapshots: Vec<QuotaSnapshotInput>,
    pub rejected_buckets: usize,
}

impl ParsedAntigravityStatus {
    pub fn quality(&self) -> Quality {
        if self.rejected_buckets == 0
            && !self.snapshots.is_empty()
            && self
                .snapshots
                .iter()
                .all(|snapshot| snapshot.quality == Quality::Fresh)
        {
            Quality::Fresh
        } else {
            Quality::Partial
        }
    }
}

/// Sanitized document-level Antigravity parser failures.
///
/// Variants deliberately carry no deserializer messages or input-derived data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AntigravityParseError {
    #[error("Antigravity status input exceeds the size limit")]
    Oversized,
    #[error("Antigravity status input is not one valid JSON object")]
    InvalidDocument,
    #[error("Antigravity status input has an unexpected product")]
    UnexpectedProduct,
}

#[derive(Deserialize)]
struct StatusEnvelope {
    product: Option<String>,
    version: Option<VersionField>,
    quota: Option<BTreeMap<String, Box<RawValue>>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VersionField {
    Text(String),
    Invalid(IgnoredAny),
}

#[derive(Deserialize)]
struct QuotaBucket {
    remaining_fraction: f64,
    reset_time: Option<String>,
    reset_in_seconds: Option<i64>,
}

/// Parses one bounded Antigravity status-line JSON object.
///
/// An absent or null `quota` member is a successful empty observation. A
/// non-object `quota` member is a document error. Quota entries are captured as
/// independent raw fragments and parsed one at a time, so one invalid bucket
/// does not discard valid siblings.
pub fn parse_status_line(
    input: &[u8],
    observed_at: i64,
) -> Result<ParsedAntigravityStatus, AntigravityParseError> {
    if input.len() > MAX_STATUS_INPUT_BYTES {
        return Err(AntigravityParseError::Oversized);
    }

    let envelope: StatusEnvelope =
        serde_json::from_slice(input).map_err(|_| AntigravityParseError::InvalidDocument)?;
    if envelope
        .product
        .as_deref()
        .is_some_and(|product| product != "antigravity")
    {
        return Err(AntigravityParseError::UnexpectedProduct);
    }

    let source_version = envelope.version.and_then(|version| match version {
        VersionField::Text(version) if is_valid_version(&version) => Some(version),
        VersionField::Text(_) | VersionField::Invalid(_) => None,
    });
    let source_sequence = Uuid::new_v4();
    let mut parsed = ParsedAntigravityStatus {
        snapshots: Vec::new(),
        rejected_buckets: 0,
    };

    let Some(quota) = envelope.quota else {
        return Ok(parsed);
    };

    for (scope_key, raw_bucket) in quota {
        if !is_valid_bucket_key(&scope_key) {
            parsed.rejected_buckets += 1;
            continue;
        }

        let bucket = match serde_json::from_str::<QuotaBucket>(raw_bucket.get()) {
            Ok(bucket) => bucket,
            Err(_) => {
                parsed.rejected_buckets += 1;
                continue;
            }
        };
        match parse_bucket(
            scope_key,
            bucket,
            observed_at,
            source_version.as_deref(),
            source_sequence,
        ) {
            Some(snapshot) => parsed.snapshots.push(snapshot),
            None => parsed.rejected_buckets += 1,
        }
    }

    Ok(parsed)
}

fn parse_bucket(
    scope_key: String,
    bucket: QuotaBucket,
    observed_at: i64,
    source_version: Option<&str>,
    source_sequence: Uuid,
) -> Option<QuotaSnapshotInput> {
    if !bucket.remaining_fraction.is_finite() || !(0.0..=1.0).contains(&bucket.remaining_fraction) {
        return None;
    }

    let absolute_reset = bucket
        .reset_time
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .map(|timestamp| timestamp.unix_timestamp());
    let relative_reset = match bucket.reset_in_seconds {
        Some(seconds) if seconds < 0 => return None,
        Some(seconds) => Some((observed_at / 1_000).checked_add(seconds)?),
        None => None,
    };
    let resets_disagree = matches!(
        (absolute_reset, relative_reset),
        (Some(absolute), Some(relative))
            if absolute.abs_diff(relative) > RESET_DISAGREEMENT_TOLERANCE_SECONDS
    );
    let resets_at = absolute_reset.or(relative_reset);
    let quality = if absolute_reset.is_some() && !resets_disagree {
        Quality::Fresh
    } else {
        Quality::Partial
    };
    let snapshot = QuotaSnapshotInput {
        provider: Provider::GoogleAntigravity,
        scope_key,
        window_kind: WindowKind::Other,
        limit_kind: LimitKind::Limited,
        window_duration_seconds: None,
        used_percent: (1.0 - bucket.remaining_fraction) * 100.0,
        resets_at,
        observed_at,
        availability: if bucket.remaining_fraction == 0.0 {
            Availability::Blocked
        } else {
            Availability::Allowed
        },
        source_version: source_version.map(str::to_owned),
        quality,
        source_sequence,
    };

    QuotaSnapshot::new(snapshot.clone()).ok().map(|_| snapshot)
}

fn is_valid_bucket_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    (1..=MAX_BUCKET_KEY_BYTES).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_valid_version(version: &str) -> bool {
    (1..=MAX_VERSION_BYTES).contains(&version.len())
        && version
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod tests;
