use ratatui::style::Color;

use crate::domain::{Provider, WindowKind};

const USAGE_BAR_WIDTH: usize = 16;
const ORANGE: Color = Color::Rgb(255, 165, 0);

pub(super) fn provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "Codex",
        Provider::Claude => "Claude",
        Provider::GitHubCopilot => "GitHub Copilot",
        Provider::GoogleAntigravity => "Google AI · Antigravity",
    }
}

pub(super) fn window_label(kind: WindowKind, duration: Option<i64>) -> String {
    match kind {
        WindowKind::Rolling5h => "5 hours".into(),
        WindowKind::Rolling7d => "7 days".into(),
        WindowKind::Spend => "Spend".into(),
        WindowKind::Monthly => "Monthly".into(),
        WindowKind::Other => duration
            .map(|value| format!("{} min", value / 60))
            .unwrap_or_else(|| "Other".into()),
    }
}

pub(super) fn humanize_scope(scope_key: &str) -> String {
    let label = scope_key.replace(['_', '-'], " ");
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => label,
    }
}

pub(super) fn format_usage(used_percent: f64) -> String {
    let clamped = used_percent.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * USAGE_BAR_WIDTH as f64).round() as usize;
    format!(
        "[{}{}] {:.1}% used, {:.1}% left",
        "█".repeat(filled),
        "░".repeat(USAGE_BAR_WIDTH - filled),
        used_percent,
        (100.0 - used_percent).clamp(0.0, 100.0)
    )
}

pub(super) fn usage_color(used_percent: f64) -> Color {
    if used_percent > 90.0 {
        Color::Red
    } else if used_percent > 75.0 {
        ORANGE
    } else if used_percent > 50.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}
