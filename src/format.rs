//! Human-readable formatting for the terminal UI.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        "-".to_string()
    } else {
        format!("{}/s", bytes(bytes_per_sec))
    }
}

pub fn eta(remaining: u64, rate: u64) -> String {
    if rate == 0 {
        return "-".into();
    }
    let s = remaining / rate;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86400, (s % 86400) / 3600),
    }
}

pub fn percent(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64 * 100.0).min(100.0)
    }
}

pub fn bar(done: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 { 0 } else { ((done as f64 / total as f64) * width as f64).round() as usize };
    let filled = filled.min(width);
    format!("[{}{}]", "#".repeat(filled), ".".repeat(width - filled))
}

/// Truncate to `max` chars with an ellipsis (char-boundary safe).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert!(bytes(u64::MAX).ends_with("TiB"));
    }

    #[test]
    fn formats_rate_and_eta() {
        assert_eq!(rate(0), "-");
        assert_eq!(rate(2048), "2.0 KiB/s");
        assert_eq!(eta(100, 0), "-");
        assert_eq!(eta(30, 1), "30s");
        assert_eq!(eta(125, 1), "2m05s");
        assert_eq!(eta(7300, 1), "2h01m");
        assert_eq!(eta(200_000, 1), "2d07h");
    }

    #[test]
    fn percent_and_bar() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(50, 200), 25.0);
        assert_eq!(percent(300, 200), 100.0);
        assert_eq!(bar(50, 100, 10), "[#####.....]");
        assert_eq!(bar(0, 0, 4), "[....]");
        assert_eq!(bar(999, 100, 4), "[####]");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("ñandú-ñandú", 6), "ñandú…");
        assert_eq!(truncate("x", 0), "…");
    }
}
