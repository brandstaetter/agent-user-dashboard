//! Presentation-only formatting for epoch values and elapsed durations.

use time::{Date, Month, OffsetDateTime};

use crate::domain::{Provider, WindowKind};

pub(crate) fn format_duration_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 86_400 {
        format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600)
    } else if seconds >= 3_600 {
        format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

pub(crate) fn format_age_millis(value_millis: i64, reference_millis: i64) -> String {
    let elapsed_millis = reference_millis.saturating_sub(value_millis).max(0);
    format!("{} ago", format_duration_seconds(elapsed_millis / 1_000))
}

pub(crate) fn format_reset_seconds(reset_seconds: Option<i64>, reference_seconds: i64) -> String {
    match reset_seconds {
        None => "unknown".into(),
        Some(reset) if reset <= reference_seconds => format!(
            "expired {} ago",
            format_duration_seconds(reference_seconds.saturating_sub(reset))
        ),
        Some(reset) => format!(
            "in {}",
            format_duration_seconds(reset.saturating_sub(reference_seconds))
        ),
    }
}

/// Formats provider reset evidence, falling back to the documented Copilot
/// calendar boundary only when no provider reset was retained.
pub(crate) fn format_snapshot_reset(
    provider: Provider,
    window_kind: WindowKind,
    provider_reset_seconds: Option<i64>,
    observed_at_millis: i64,
    reference_seconds: i64,
) -> String {
    if provider_reset_seconds.is_some() {
        return format_reset_seconds(provider_reset_seconds, reference_seconds);
    }
    if provider != Provider::GitHubCopilot || window_kind != WindowKind::Monthly {
        return format_reset_seconds(None, reference_seconds);
    }

    match next_month_start_seconds(observed_at_millis) {
        None => "unknown".into(),
        Some(expected) if expected <= reference_seconds => format!(
            "expected {} ago",
            format_duration_seconds(reference_seconds.saturating_sub(expected))
        ),
        Some(expected) => format!(
            "expected in {}",
            format_duration_seconds(expected.saturating_sub(reference_seconds))
        ),
    }
}

fn next_month_start_seconds(observed_at_millis: i64) -> Option<i64> {
    if observed_at_millis < 0 {
        return None;
    }
    let observed = OffsetDateTime::from_unix_timestamp(observed_at_millis / 1_000).ok()?;
    let (year, month) = match observed.month() {
        Month::January => (observed.year(), Month::February),
        Month::February => (observed.year(), Month::March),
        Month::March => (observed.year(), Month::April),
        Month::April => (observed.year(), Month::May),
        Month::May => (observed.year(), Month::June),
        Month::June => (observed.year(), Month::July),
        Month::July => (observed.year(), Month::August),
        Month::August => (observed.year(), Month::September),
        Month::September => (observed.year(), Month::October),
        Month::October => (observed.year(), Month::November),
        Month::November => (observed.year(), Month::December),
        Month::December => (observed.year().checked_add(1)?, Month::January),
    };
    Some(
        Date::from_calendar_date(year, month, 1)
            .ok()?
            .midnight()
            .assume_utc()
            .unix_timestamp(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_seconds(year: i32, month: Month, day: u8, hour: u8) -> i64 {
        Date::from_calendar_date(year, month, day)
            .unwrap()
            .with_hms(hour, 0, 0)
            .unwrap()
            .assume_utc()
            .unix_timestamp()
    }

    #[test]
    fn duration_formats_seconds_minutes_hours_and_days() {
        assert_eq!(format_duration_seconds(42), "42s");
        assert_eq!(format_duration_seconds(125), "2m 5s");
        assert_eq!(format_duration_seconds(7_380), "2h 3m");
        assert_eq!(format_duration_seconds(183_600), "2d 3h");
    }

    #[test]
    fn reset_formats_future_expired_and_unknown_values() {
        assert_eq!(format_reset_seconds(Some(3_725), 3_600), "in 2m 5s");
        assert_eq!(
            format_reset_seconds(Some(3_475), 3_600),
            "expired 2m 5s ago"
        );
        assert_eq!(format_reset_seconds(None, 3_600), "unknown");
    }

    #[test]
    fn ages_and_durations_saturate_clock_skew_to_zero() {
        assert_eq!(format_duration_seconds(-1), "0s");
        assert_eq!(format_age_millis(3_601_000, 3_600_000), "0s ago");
    }

    #[test]
    fn copilot_monthly_missing_reset_uses_expected_next_calendar_month() {
        let observed = utc_seconds(2026, Month::September, 20, 12);
        let expected = utc_seconds(2026, Month::October, 1, 0);
        assert_eq!(next_month_start_seconds(observed * 1_000), Some(expected));
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                None,
                observed * 1_000,
                observed,
            ),
            "expected in 10d 12h"
        );
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                None,
                observed * 1_000,
                expected + 90_000,
            ),
            "expected 1d 1h ago"
        );
    }

    #[test]
    fn calendar_fallback_advances_at_exact_boundary_and_across_year_end() {
        let october = utc_seconds(2026, Month::October, 1, 0);
        let november = utc_seconds(2026, Month::November, 1, 0);
        assert_eq!(next_month_start_seconds(october * 1_000), Some(november));

        let december = utc_seconds(2026, Month::December, 31, 23);
        let january = utc_seconds(2027, Month::January, 1, 0);
        assert_eq!(next_month_start_seconds(december * 1_000), Some(january));
    }

    #[test]
    fn calendar_fallback_handles_leap_year_february() {
        let february = utc_seconds(2028, Month::February, 29, 12);
        let march = utc_seconds(2028, Month::March, 1, 0);
        assert_eq!(next_month_start_seconds(february * 1_000), Some(march));
    }

    #[test]
    fn provider_reset_always_wins_even_when_expired_or_off_policy() {
        let observed = utc_seconds(2026, Month::September, 20, 12);
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                Some(observed + 125),
                observed * 1_000,
                observed,
            ),
            "in 2m 5s"
        );
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                Some(observed - 125),
                observed * 1_000,
                observed,
            ),
            "expired 2m 5s ago"
        );
    }

    #[test]
    fn fallback_excludes_unrelated_rows_and_fails_closed() {
        let observed = utc_seconds(2026, Month::September, 20, 12);
        for (provider, window) in [
            (Provider::Codex, WindowKind::Monthly),
            (Provider::Claude, WindowKind::Monthly),
            (Provider::GitHubCopilot, WindowKind::Other),
        ] {
            assert_eq!(
                format_snapshot_reset(provider, window, None, observed * 1_000, observed),
                "unknown"
            );
        }
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                None,
                i64::MAX,
                observed,
            ),
            "unknown"
        );
        assert_eq!(
            format_snapshot_reset(
                Provider::GitHubCopilot,
                WindowKind::Monthly,
                None,
                -1,
                observed,
            ),
            "unknown"
        );
    }
}
