use chrono::{DateTime, Utc};

pub(super) fn format_temperature(value: f64) -> String {
    format!("{}C", value.round() as i64)
}

pub(super) fn format_percent(value: f64) -> String {
    format!("{}%", value.clamp(0.0, 100.0))
}

pub(super) fn progress_number(value: f64) -> Option<f64> {
    value.is_finite().then(|| value.clamp(0.0, 100.0))
}

pub(super) fn format_seconds(seconds: f64) -> String {
    let total_seconds = seconds as i64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let remaining_seconds = total_seconds % 60;
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if remaining_seconds > 0 || parts.is_empty() {
        parts.push(format!("{remaining_seconds}s"));
    }
    parts.join(" ")
}

pub(super) fn format_weight(value: &str) -> Option<String> {
    let grams = value.trim().parse::<f64>().ok()?;
    if grams >= 1000.0 {
        Some(format!("{:.1}kg", grams / 1000.0))
    } else {
        Some(format!("{grams:.1}g").replace(".0g", "g"))
    }
}

pub(super) fn parse_bambu_datetime(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .or_else(|_| DateTime::parse_from_rfc3339(&text.replace('Z', "+00:00")))
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}
