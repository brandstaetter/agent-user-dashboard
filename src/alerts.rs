//! Deterministic alert evaluation over normalized, persisted observations.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    config::AppConfig,
    domain::{LimitKind, Provider, Quality, QuotaSnapshot, WindowKind},
    storage::{Storage, StorageError},
    time_format::format_reset_seconds,
};

const MATERIAL_RESET_ADVANCE_SECONDS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    LowRemaining,
    ResetSoon,
    ResetObserved,
}

impl AlertKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowRemaining => "low_remaining",
            Self::ResetSoon => "reset_soon",
            Self::ResetObserved => "reset_observed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    pub provider: Provider,
    pub scope_key: String,
    pub window_kind: WindowKind,
    pub kind: AlertKind,
    pub cycle_key: String,
    pub fired_at: i64,
    pub remaining_percent: f64,
    pub resets_at: Option<i64>,
}

impl AlertEvent {
    pub fn message(&self) -> String {
        let scope = display_scope(self.window_kind, &self.scope_key);
        let subject = format!("{} {} ({})", self.provider, self.window_kind, scope);
        match self.kind {
            AlertKind::LowRemaining => {
                format!("{subject} has {:.1}% remaining", self.remaining_percent)
            }
            AlertKind::ResetSoon => format!(
                "{subject} resets {}",
                format_reset_seconds(self.resets_at, self.fired_at / 1_000)
            ),
            AlertKind::ResetObserved => format!(
                "{subject} entered a new reset cycle with {:.1}% remaining",
                self.remaining_percent
            ),
        }
    }
}

fn display_scope(kind: WindowKind, scope_key: &str) -> String {
    if !matches!(kind, WindowKind::Monthly | WindowKind::Other) {
        return scope_key.to_owned();
    }
    let label = scope_key.replace(['_', '-'], " ");
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => label,
    }
}

pub trait Clock {
    fn now_millis(&self) -> i64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(i64::MAX)
    }
}

#[derive(Debug, Clone)]
pub struct AlertEngine {
    config: AppConfig,
}

impl AlertEngine {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub fn process(
        &self,
        storage: &Storage,
        current: &[QuotaSnapshot],
        clock: &impl Clock,
    ) -> Result<Vec<AlertEvent>, StorageError> {
        let now = clock.now_millis();
        let mut emitted = Vec::new();
        for snapshot in current {
            let row = snapshot.as_input();
            if !self.is_current_fresh(snapshot, now) {
                continue;
            }
            let previous = storage.previous_snapshot_before(
                row.provider,
                &row.scope_key,
                row.window_kind,
                row.observed_at,
            )?;
            for kind in self.evaluate(previous.as_ref().map(|item| &item.snapshot), snapshot, now) {
                let cycle_key = cycle_key(snapshot);
                if storage.claim_alert(
                    row.provider,
                    &row.scope_key,
                    row.window_kind,
                    kind.as_str(),
                    &cycle_key,
                    now,
                )? {
                    emitted.push(AlertEvent {
                        provider: row.provider,
                        scope_key: row.scope_key.clone(),
                        window_kind: row.window_kind,
                        kind,
                        cycle_key,
                        fired_at: now,
                        remaining_percent: snapshot.remaining_percent(),
                        resets_at: row.resets_at,
                    });
                }
            }
        }
        Ok(emitted)
    }

    fn evaluate(
        &self,
        previous: Option<&QuotaSnapshot>,
        current: &QuotaSnapshot,
        now_millis: i64,
    ) -> Vec<AlertKind> {
        let mut kinds = Vec::new();
        let row = current.as_input();
        if row.limit_kind == LimitKind::Unlimited {
            return kinds;
        }
        if let Some(previous) = previous
            && previous.as_input().quality == Quality::Fresh
            && previous.as_input().limit_kind == LimitKind::Limited
        {
            let prior = previous.as_input();
            let same_cycle = prior.resets_at == row.resets_at;
            if same_cycle
                && previous.remaining_percent() > self.config.low_remaining_percent
                && current.remaining_percent() <= self.config.low_remaining_percent
            {
                kinds.push(AlertKind::LowRemaining);
            }
            if reset_advanced(prior.resets_at, row.resets_at)
                && row.used_percent < prior.used_percent
            {
                kinds.push(AlertKind::ResetObserved);
            }
        }
        if reset_is_soon(row.resets_at, now_millis, self.config.reset_soon_seconds) {
            kinds.push(AlertKind::ResetSoon);
        }
        kinds
    }

    fn is_current_fresh(&self, snapshot: &QuotaSnapshot, now_millis: i64) -> bool {
        let row = snapshot.as_input();
        if row.quality != Quality::Fresh {
            return false;
        }
        if row
            .resets_at
            .is_some_and(|reset| reset <= now_millis / 1_000)
        {
            return false;
        }
        let stale_after = match row.provider {
            Provider::Codex => (self.config.codex_refresh_seconds * 2).max(180),
            Provider::Claude => self.config.claude_stale_after_seconds,
            Provider::GitHubCopilot => (self.config.copilot_refresh_seconds * 2).max(180),
            Provider::GoogleAntigravity => self.config.antigravity_stale_after_seconds,
        };
        now_millis.saturating_sub(row.observed_at) <= stale_after as i64 * 1_000
    }
}

fn reset_advanced(previous: Option<i64>, current: Option<i64>) -> bool {
    matches!((previous, current), (Some(old), Some(new))
        if new.saturating_sub(old) >= MATERIAL_RESET_ADVANCE_SECONDS)
}

fn reset_is_soon(reset: Option<i64>, now_millis: i64, horizon_seconds: u64) -> bool {
    let now = now_millis / 1_000;
    reset.is_some_and(|value| value > now && value.saturating_sub(now) <= horizon_seconds as i64)
}

fn cycle_key(snapshot: &QuotaSnapshot) -> String {
    let row = snapshot.as_input();
    row.resets_at.map_or_else(
        || format!("observed-{}", row.observed_at / 3_600_000),
        |reset| reset.to_string(),
    )
}

#[cfg(test)]
#[path = "alerts_tests.rs"]
mod tests;
