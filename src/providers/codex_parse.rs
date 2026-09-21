use std::collections::HashSet;

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::{
    Availability, LimitKind, Provider, Quality, QuotaSnapshot, QuotaSnapshotInput, WindowKind,
};

use super::CodexObservation;

pub fn parse_rate_limits_result(
    result: &Value,
    observed_at: i64,
    source_version: Option<&str>,
) -> CodexObservation {
    let sequence = Uuid::new_v4();
    let mut observation = CodexObservation {
        snapshots: Vec::new(),
        rejected_windows: 0,
    };
    let Some(result) = result.as_object() else {
        observation.rejected_windows = 1;
        return observation;
    };

    let by_limit = result.get("rateLimitsByLimitId").and_then(Value::as_object);
    if let Some(buckets) = by_limit {
        for (limit_id, snapshot) in buckets {
            parse_snapshot(
                snapshot,
                &sanitize_scope_key(limit_id),
                observed_at,
                source_version,
                sequence,
                &mut observation,
            );
        }
    }
    if observation.snapshots.is_empty()
        && let Some(snapshot) = result.get("rateLimits")
    {
        parse_snapshot(
            snapshot,
            "subscription",
            observed_at,
            source_version,
            sequence,
            &mut observation,
        );
    }
    deduplicate(&mut observation.snapshots);
    observation
}

fn parse_snapshot(
    value: &Value,
    scope_key: &str,
    observed_at: i64,
    source_version: Option<&str>,
    source_sequence: Uuid,
    observation: &mut CodexObservation,
) {
    let Some(snapshot) = value.as_object() else {
        observation.rejected_windows += 1;
        return;
    };
    for slot in ["primary", "secondary"] {
        let Some(window) = snapshot.get(slot) else {
            continue;
        };
        if window.is_null() {
            continue;
        }
        match parse_window(
            window,
            snapshot,
            scope_key,
            observed_at,
            source_version,
            source_sequence,
        ) {
            Some(row) => observation.snapshots.push(row),
            None => observation.rejected_windows += 1,
        }
    }
}

fn parse_window(
    value: &Value,
    snapshot: &Map<String, Value>,
    scope_key: &str,
    observed_at: i64,
    source_version: Option<&str>,
    source_sequence: Uuid,
) -> Option<QuotaSnapshotInput> {
    let window = value.as_object()?;
    let used_percent = window.get("usedPercent")?.as_f64()?;
    if !used_percent.is_finite() {
        return None;
    }
    let duration_minutes = optional_nonnegative_integer(window.get("windowDurationMins"))?;
    let window_duration_seconds = duration_minutes.and_then(|minutes| minutes.checked_mul(60));
    if duration_minutes.is_some() && window_duration_seconds.is_none() {
        return None;
    }
    let resets_at = optional_nonnegative_integer(window.get("resetsAt"))?;
    let window_kind = match duration_minutes {
        Some(300) => WindowKind::Rolling5h,
        Some(10_080) => WindowKind::Rolling7d,
        _ => WindowKind::Other,
    };
    let availability = match window.get("reached").and_then(Value::as_bool) {
        Some(true) => Availability::Blocked,
        Some(false) => Availability::Allowed,
        None => match snapshot
            .get("ordinaryUsageAllowed")
            .and_then(Value::as_bool)
        {
            Some(true) => Availability::Allowed,
            Some(false) => Availability::Blocked,
            None => Availability::Unknown,
        },
    };
    let quality = if duration_minutes.is_some() && resets_at.is_some() {
        Quality::Fresh
    } else {
        Quality::Partial
    };
    let input = QuotaSnapshotInput {
        provider: Provider::Codex,
        scope_key: scope_key.to_owned(),
        window_kind,
        limit_kind: LimitKind::Limited,
        window_duration_seconds,
        used_percent,
        resets_at,
        observed_at,
        availability,
        source_version: source_version.map(str::to_owned),
        quality,
        source_sequence,
    };
    QuotaSnapshot::new(input.clone()).ok().map(|_| input)
}

fn optional_nonnegative_integer(value: Option<&Value>) -> Option<Option<i64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => value.as_i64().filter(|number| *number >= 0).map(Some),
    }
}

fn sanitize_scope_key(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(128));
    for character in value.chars() {
        if sanitized.len() >= 128 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "subscription".into()
    } else {
        trimmed.to_owned()
    }
}

fn deduplicate(rows: &mut Vec<QuotaSnapshotInput>) {
    let mut seen = HashSet::new();
    rows.retain(|row| {
        seen.insert((
            row.scope_key.clone(),
            row.window_kind,
            row.window_duration_seconds,
            row.used_percent.to_bits(),
            row.resets_at,
            row.availability,
        ))
    });
}
