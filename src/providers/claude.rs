use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    Availability, LimitKind, Provider, Quality, QuotaSnapshot, QuotaSnapshotInput, WindowKind,
};

pub const MAX_STATUS_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeObservation {
    pub snapshots: Vec<QuotaSnapshotInput>,
    pub rejected_windows: usize,
}

impl ClaudeObservation {
    pub fn quality(&self) -> Quality {
        if self.rejected_windows == 0
            && !self.snapshots.is_empty()
            && self
                .snapshots
                .iter()
                .all(|row| row.quality == Quality::Fresh)
        {
            Quality::Fresh
        } else {
            Quality::Partial
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClaudeParseError {
    #[error("Claude status input exceeds the size limit")]
    Oversized,
    #[error("Claude status input is not one valid JSON object")]
    InvalidDocument,
}

/// Parses one Claude status-line object and immediately discards unrelated data.
pub fn parse_status_line(
    input: &[u8],
    observed_at: i64,
) -> Result<ClaudeObservation, ClaudeParseError> {
    if input.len() > MAX_STATUS_INPUT_BYTES {
        return Err(ClaudeParseError::Oversized);
    }
    let root: Value =
        serde_json::from_slice(input).map_err(|_| ClaudeParseError::InvalidDocument)?;
    let root = root.as_object().ok_or(ClaudeParseError::InvalidDocument)?;
    let rate_limits = root.get("rate_limits").and_then(Value::as_object);
    let sequence = Uuid::new_v4();
    let mut observation = ClaudeObservation {
        snapshots: Vec::new(),
        rejected_windows: 0,
    };

    let Some(rate_limits) = rate_limits else {
        return Ok(observation);
    };
    for (name, kind) in [
        ("five_hour", WindowKind::Rolling5h),
        ("seven_day", WindowKind::Rolling7d),
        ("spend_limit", WindowKind::Spend),
    ] {
        let Some(value) = rate_limits.get(name) else {
            continue;
        };
        match parse_window(value, kind, observed_at, sequence) {
            Some(row) => observation.snapshots.push(row),
            None => observation.rejected_windows += 1,
        }
    }
    Ok(observation)
}

fn parse_window(
    value: &Value,
    window_kind: WindowKind,
    observed_at: i64,
    source_sequence: Uuid,
) -> Option<QuotaSnapshotInput> {
    let object = value.as_object()?;
    let used_percent = object.get("used_percentage")?.as_f64()?;
    if !used_percent.is_finite() {
        return None;
    }
    let resets_at = match object.get("resets_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64().filter(|timestamp| *timestamp >= 0)?),
    };
    let quality = if resets_at.is_some() {
        Quality::Fresh
    } else {
        Quality::Partial
    };
    let input = QuotaSnapshotInput {
        provider: Provider::Claude,
        scope_key: "subscription".into(),
        window_kind,
        limit_kind: LimitKind::Limited,
        window_duration_seconds: match window_kind {
            WindowKind::Rolling5h => Some(5 * 60 * 60),
            WindowKind::Rolling7d => Some(7 * 24 * 60 * 60),
            WindowKind::Spend | WindowKind::Monthly | WindowKind::Other => None,
        },
        used_percent,
        resets_at,
        observed_at,
        availability: Availability::Unknown,
        source_version: None,
        quality,
        source_sequence,
    };
    QuotaSnapshot::new(input.clone()).ok().map(|_| input)
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
