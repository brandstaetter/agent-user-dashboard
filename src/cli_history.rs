use std::io::Write;

use super::CliError;
use crate::{
    domain::{LimitKind, WindowKind},
    storage::Storage,
    time_format::{format_age_millis, format_snapshot_reset},
};

pub(super) fn write(
    storage: &Storage,
    mut writer: impl Write,
    limit: usize,
    reference_millis: i64,
) -> Result<(), CliError> {
    writeln!(writer, "Snapshots (newest first):")?;
    for stored in storage.recent_history(limit)? {
        let row = stored.snapshot.as_input();
        writeln!(
            writer,
            "{} {} {} observed={} usage={} reset={} quality={}",
            row.provider,
            history_window_label(row.window_kind),
            history_scope_label(row.window_kind, &row.scope_key),
            format_age_millis(row.observed_at, reference_millis),
            match row.limit_kind {
                LimitKind::Limited => format!(
                    "{:.1}% used, {:.1}% left",
                    row.used_percent,
                    stored.snapshot.remaining_percent()
                ),
                LimitKind::Unlimited => "Unlimited".into(),
            },
            format_snapshot_reset(
                row.provider,
                row.window_kind,
                row.resets_at,
                row.observed_at,
                reference_millis / 1_000,
            ),
            row.quality
        )?;
    }
    writeln!(writer, "Alerts (newest first):")?;
    for alert in storage.recent_alerts(limit)? {
        writeln!(
            writer,
            "{} {} {} kind={} fired={}",
            alert.provider,
            alert.window_kind,
            alert.scope_key,
            alert.alert_kind,
            format_age_millis(alert.fired_at, reference_millis)
        )?;
    }
    Ok(())
}

fn history_window_label(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Rolling5h => "5 hours",
        WindowKind::Rolling7d => "7 days",
        WindowKind::Spend => "Spend",
        WindowKind::Monthly => "Monthly",
        WindowKind::Other => "Other",
    }
}

fn history_scope_label(kind: WindowKind, scope_key: &str) -> String {
    if !matches!(kind, WindowKind::Monthly | WindowKind::Other) {
        return scope_key.to_owned();
    }
    humanize_scope(scope_key)
}

fn humanize_scope(scope_key: &str) -> String {
    let label = scope_key.replace(['_', '-'], " ");
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => label,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{Availability, Provider, Quality, QuotaSnapshot, QuotaSnapshotInput};

    const NOW: i64 = 1_700_000_000_000;

    fn fixture() -> (TempDir, Storage) {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::open(temp.path().join("usage.sqlite3")).unwrap();
        (temp, storage)
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        provider: Provider,
        scope_key: &str,
        window_kind: WindowKind,
        limit_kind: LimitKind,
        used_percent: f64,
        resets_at: Option<i64>,
        quality: Quality,
    ) -> QuotaSnapshot {
        QuotaSnapshot::new(QuotaSnapshotInput {
            provider,
            scope_key: scope_key.into(),
            window_kind,
            limit_kind,
            window_duration_seconds: match window_kind {
                WindowKind::Rolling5h => Some(18_000),
                WindowKind::Rolling7d => Some(604_800),
                _ => None,
            },
            used_percent,
            resets_at,
            observed_at: NOW,
            availability: Availability::Allowed,
            source_version: None,
            quality,
            source_sequence: Uuid::new_v4(),
        })
        .unwrap()
    }

    #[test]
    fn copilot_history_humanizes_categories_and_distinguishes_limit_kinds() {
        let (_temp, storage) = fixture();
        storage
            .insert_snapshots(&[
                row(
                    Provider::GitHubCopilot,
                    "premium_interactions",
                    WindowKind::Monthly,
                    LimitKind::Limited,
                    25.0,
                    Some(NOW / 1_000 + 3_600),
                    Quality::Fresh,
                ),
                row(
                    Provider::GitHubCopilot,
                    "chat",
                    WindowKind::Monthly,
                    LimitKind::Unlimited,
                    0.0,
                    Some(NOW / 1_000 + 3_600),
                    Quality::Fresh,
                ),
            ])
            .unwrap();

        let mut output = Vec::new();
        write(&storage, &mut output, 10, NOW).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains(
            "GitHub Copilot Monthly Premium interactions observed=0s ago usage=25.0% used, 75.0% left"
        ));
        assert!(output.contains("GitHub Copilot Monthly Chat observed=0s ago usage=Unlimited"));
        let unlimited_line = output.lines().find(|line| line.contains("Chat")).unwrap();
        assert!(!unlimited_line.contains('%'));
        assert!(!output.contains("premium_interactions"));
    }

    #[test]
    fn history_distinguishes_expected_provider_and_unknown_resets() {
        let (_temp, storage) = fixture();
        storage
            .insert_snapshots(&[
                row(
                    Provider::GitHubCopilot,
                    "missing_reset",
                    WindowKind::Monthly,
                    LimitKind::Limited,
                    25.0,
                    None,
                    Quality::Partial,
                ),
                row(
                    Provider::GitHubCopilot,
                    "provider_reset",
                    WindowKind::Monthly,
                    LimitKind::Limited,
                    25.0,
                    Some(NOW / 1_000 + 3_600),
                    Quality::Fresh,
                ),
                row(
                    Provider::Claude,
                    "subscription",
                    WindowKind::Rolling7d,
                    LimitKind::Limited,
                    25.0,
                    None,
                    Quality::Partial,
                ),
            ])
            .unwrap();

        let mut output = Vec::new();
        write(&storage, &mut output, 10, NOW).unwrap();
        let output = String::from_utf8(output).unwrap();
        let expected = output
            .lines()
            .find(|line| line.contains("Missing reset"))
            .unwrap();
        let provider = output
            .lines()
            .find(|line| line.contains("Provider reset"))
            .unwrap();
        let unrelated = output
            .lines()
            .find(|line| line.starts_with("Claude "))
            .unwrap();

        assert!(expected.contains("reset=expected in "));
        assert!(provider.contains("reset=in 1h 0m"));
        assert!(!provider.contains("expected"));
        assert!(unrelated.contains("reset=unknown"));
    }

    #[test]
    fn antigravity_history_humanizes_dynamic_other_scopes() {
        let (_temp, storage) = fixture();
        storage
            .insert_snapshots(&[
                row(
                    Provider::GoogleAntigravity,
                    "gemini-weekly",
                    WindowKind::Other,
                    LimitKind::Limited,
                    25.0,
                    Some(NOW / 1_000 + 3_600),
                    Quality::Fresh,
                ),
                row(
                    Provider::GoogleAntigravity,
                    "pro_model",
                    WindowKind::Other,
                    LimitKind::Limited,
                    100.0,
                    Some(NOW / 1_000 + 7_200),
                    Quality::Fresh,
                ),
            ])
            .unwrap();

        let mut output = Vec::new();
        write(&storage, &mut output, 10, NOW).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains(&format!(
            "{} Other Gemini weekly observed=0s ago usage=25.0% used, 75.0% left",
            Provider::GoogleAntigravity
        )));
        assert!(output.contains(&format!(
            "{} Other Pro model observed=0s ago usage=100.0% used, 0.0% left",
            Provider::GoogleAntigravity
        )));
        assert!(!output.contains("gemini-weekly"));
        assert!(!output.contains("pro_model"));
    }
}
