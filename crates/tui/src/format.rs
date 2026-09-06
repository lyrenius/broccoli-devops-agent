//! Text helpers shared by the screens: labels, times, sizes, colors, and wrapping.

use chrono::{DateTime, Local, Utc};
use ratatui::style::Color;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::UsageTotals;

/// The first eight characters of an ID, with an ellipsis when there is more.
pub fn short(id: &str) -> String {
    let head: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        format!("{head}…")
    } else {
        head
    }
}

/// Display name of an enum value from the API: `waiting_for_human` reads as `waiting for human`.
pub fn label(value: &str) -> String {
    value.replace('_', " ")
}

/// Display name of a resource kind; the enum values are protocol, these are for people.
pub fn kind_name(kind: &str) -> String {
    match kind {
        "postgre_sql" => "PostgreSQL",
        "redis" => "Redis",
        "object_storage" => "Object storage",
        "broccoli_server" => "Broccoli server",
        "frontend" => "Frontend",
        "gateway" => "Gateway",
        "worker" => "Judge worker",
        "printer_station" => "Printer station",
        "balloon_station" => "Balloon station",
        "printer" => "Printer",
        "network_vantage" => "Network vantage",
        "agent_control_plane" => "Control plane",
        other => return label(other),
    }
    .to_string()
}

/// Parses an RFC 3339 timestamp as the API writes them.
pub fn parse_time(iso: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Milliseconds since the epoch of an RFC 3339 timestamp.
pub fn millis(iso: &str) -> Option<i64> {
    parse_time(iso).map(|time| time.timestamp_millis())
}

/// Local wall-clock time of a timestamp, `HH:MM:SS`.
pub fn time(iso: &str) -> String {
    parse_time(iso).map_or_else(
        || iso.get(11..19).unwrap_or(iso).to_string(),
        |time| time.with_timezone(&Local).format("%H:%M:%S").to_string(),
    )
}

/// Local date and time of a timestamp.
pub fn date_time(iso: &str) -> String {
    parse_time(iso).map_or_else(
        || iso.to_string(),
        |time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        },
    )
}

/// How long ago a timestamp was, the way the web console says it.
pub fn age(iso: &str, now: DateTime<Utc>) -> String {
    let Some(then) = parse_time(iso) else {
        return iso.to_string();
    };
    let seconds = (now - then).num_seconds().max(0);
    if seconds < 5 {
        "just now".to_string()
    } else if seconds < 90 {
        format!("{seconds}s ago")
    } else if seconds < 5400 {
        format!("{} min ago", (seconds as f64 / 60.0).round())
    } else if seconds < 172_800 {
        format!("{} h ago", (seconds as f64 / 3600.0).round())
    } else {
        format!("{} d ago", (seconds as f64 / 86_400.0).round())
    }
}

/// A duration in milliseconds, at the precision a human reads it at.
pub fn duration_ms(ms: i64) -> String {
    if ms < 0 {
        "—".to_string()
    } else if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// A duration in seconds, for uptimes.
pub fn duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{} d {} h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Compact token counts: 1_234_567 reads as 1.23M, which is what a bill is discussed in.
pub fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// A count with thousands separators.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Money is only ever shown when a price list exists; tokens are shown regardless.
pub fn money(usage: &UsageTotals) -> Option<String> {
    usage.cost.map(|cost| {
        format!("{cost:.4} {}", usage.currency.as_deref().unwrap_or(""))
            .trim_end()
            .to_string()
    })
}

/// Semantic color for a health, status, mode, or approval string, as the web console's badges.
pub fn tone(value: &str) -> Color {
    match value {
        "healthy" | "succeeded" | "running" | "resolved" | "completed" | "approved"
        | "not_required" | "solved" | "strong" => Color::Green,
        "degraded"
        | "waiting_for_approval"
        | "waiting_for_human"
        | "dispatch_frozen"
        | "pending"
        | "verifying"
        | "mitigating"
        | "investigating"
        | "open"
        | "queued"
        | "needs_resnapshot"
        | "needs_more_data"
        | "needs_human"
        | "weak"
        | "diagnosis_only" => Color::Yellow,
        "down"
        | "failed"
        | "verification_failed"
        | "cancelled"
        | "fully_frozen"
        | "rejected"
        | "recovering"
        | "blocked" => Color::Red,
        _ => Color::DarkGray,
    }
}

/// Color of an event's actor, as the web console's log.
pub fn actor_color(actor: &str) -> Color {
    match actor {
        "human" => Color::Green,
        "agent-team" | "scheduler-policy" => Color::Magenta,
        "agents-platform" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

/// A JSON value as text: strings as they are, everything else pretty-printed.
pub fn pretty(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Display width of a string in terminal cells.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Cuts a string to a display width, with an ellipsis when something was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let budget = max.saturating_sub(1);
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// Cuts or pads a string to exactly a display width.
pub fn pad(text: &str, max: usize) -> String {
    let cut = truncate(text, max);
    let missing = max.saturating_sub(width(&cut));
    format!("{cut}{}", " ".repeat(missing))
}

/// Word-wraps text to a display width, keeping explicit line breaks and hard-breaking words
/// that are wider than a line (JSON blobs, long IDs).
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim_end_matches('\r');
        let mut line = String::new();
        let mut line_w = 0;
        for word in paragraph.split(' ') {
            let word_w = width(word);
            if word_w > max {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let mut piece = String::new();
                let mut piece_w = 0;
                for ch in word.chars() {
                    let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if piece_w + ch_w > max {
                        out.push(std::mem::take(&mut piece));
                        piece_w = 0;
                    }
                    piece.push(ch);
                    piece_w += ch_w;
                }
                line = piece;
                line_w = piece_w;
                continue;
            }
            if line.is_empty() && line_w == 0 {
                line.push_str(word);
                line_w = word_w;
            } else if line_w + 1 + word_w <= max {
                line.push(' ');
                line.push_str(word);
                line_w += 1 + word_w;
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
                line_w = word_w;
            }
        }
        out.push(line);
    }
    out
}

/// The first `lines` lines of a text, and how many were left out.
pub fn clamp(lines: Vec<String>, keep: usize) -> (Vec<String>, usize) {
    if lines.len() <= keep {
        return (lines, 0);
    }
    let hidden = lines.len() - keep;
    let mut shown = lines;
    shown.truncate(keep);
    (shown, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_and_hard_breaks_long_ones() {
        assert_eq!(wrap("the queue is stuck", 9), vec!["the queue", "is stuck"]);
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd", "efgh", "ij"],
            "a word wider than the line is cut"
        );
        assert_eq!(
            wrap("a\n\nb", 10),
            vec!["a", "", "b"],
            "blank lines survive"
        );
        assert_eq!(
            wrap("队列卡住了 很久", 10),
            vec!["队列卡住了", "很久"],
            "wide chars count double"
        );
        assert_eq!(
            wrap("队列卡住了", 6),
            vec!["队列卡", "住了"],
            "a wide word wider than the line is cut by cells"
        );
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(pad("hi", 4), "hi  ");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(tokens(1_234_567), "1.23M");
        assert_eq!(short("0198c936-5f2a-7000-8000-4a6f8c2d9dd1"), "0198c936…");
    }

    #[test]
    fn ages_and_durations_read_like_the_web_console() {
        let now = parse_time("2026-09-06T10:00:00Z").unwrap();
        assert_eq!(age("2026-09-06T09:59:58Z", now), "just now");
        assert_eq!(age("2026-09-06T09:59:00Z", now), "60s ago");
        assert_eq!(age("2026-09-06T09:30:00Z", now), "30 min ago");
        assert_eq!(age("2026-09-05T10:00:00Z", now), "24 h ago");
        assert_eq!(duration_ms(420), "420 ms");
        assert_eq!(duration_ms(1500), "1.5 s");
        assert_eq!(duration_ms(125_000), "2 min 5 s");
        assert_eq!(duration_secs(3720), "1 h 2 min");
    }
}
