use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BAR_WIDTH: usize = 20;
const EIGHTH: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

#[derive(Debug, Clone)]
pub struct Gauge {
    pub id: String,
    pub label: String,
    pub percent: u32,
    pub reset: Option<String>,
    /// Length of the quota window. Absent when the payload does not say.
    pub window_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProviderView {
    pub key: String,
    pub name: String,
    pub mark: String,
    pub plan: Option<String>,
    pub windows: Vec<Gauge>,
    pub error: Option<String>,
}

pub fn render_bar(percent: u32) -> String {
    let p = percent.min(100);
    let filled = (p as f64 / 100.0) * (BAR_WIDTH as f64);
    let mut whole = filled as usize;
    let mut eighth = ((filled - whole as f64) * 8.0 + 1e-9) as usize;
    if eighth >= 8 {
        whole += 1;
        eighth = 0;
    }
    if whole >= BAR_WIDTH {
        return "█".repeat(BAR_WIDTH);
    }
    let mut parts = "█".repeat(whole);
    if eighth > 0 {
        parts.push_str(EIGHTH[eighth - 1]);
    }
    let chars_len = parts.chars().count();
    let empty_fill = if chars_len < BAR_WIDTH {
        "░".repeat(BAR_WIDTH - chars_len)
    } else {
        String::new()
    };
    parts.push_str(&empty_fill);
    parts
}

pub fn meter_ansi_code(percent: u32) -> &'static str {
    if percent >= 80 {
        "38;2;220;70;70" // Red
    } else if percent >= 50 {
        "38;2;220;150;40" // Orange
    } else {
        "38;2;80;180;120" // Green
    }
}

pub fn paint(code: &str, text: &str, tty: bool) -> String {
    if tty {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

const WINDOW_WORDS: [(&str, &str); 3] = [("týždeň", "week"), ("mesiac", "month"), ("5 h", "5h")];

pub fn window_name(g: &Gauge) -> String {
    let raw = if !g.label.is_empty() { &g.label } else { &g.id };
    let (left, right) = if let Some(pos) = raw.find('·') {
        (raw[..pos].trim(), raw[pos + '·'.len_utf8()..].trim())
    } else {
        ("", raw.trim())
    };

    let mut norm = right.to_string();
    for (src, dst) in WINDOW_WORDS {
        norm = norm.replace(src, dst);
    }

    let prefix = match left {
        "Gemini" => "gemini ",
        "Other models" => "other models ",
        other
            if (g.id.starts_with("antigravity:") || g.id.starts_with("gemini:"))
                && !other.is_empty() =>
        {
            return format!("{} {}", other, norm).trim().to_string();
        }
        _ => "",
    };

    format!("{}{}", prefix, norm).trim().to_string()
}

fn sort_key(view: &ProviderView) -> (u8, u32, String) {
    if view.windows.is_empty() {
        return (2, 0, view.name.clone());
    }
    let max_used = view.windows.iter().map(|g| g.percent).max().unwrap_or(0);
    (0, max_used, view.name.clone())
}

pub fn render_blocks(views: &[ProviderView], tty: bool) -> String {
    let mut sorted_views: Vec<ProviderView> = views.to_vec();
    sorted_views.sort_by_key(sort_key);

    let mut chosen: Vec<&ProviderView> = Vec::new();
    for v in &sorted_views {
        if !v.windows.is_empty() || v.error.is_some() {
            chosen.push(v);
        }
    }
    if chosen.is_empty() {
        return String::new();
    }

    let mut max_width = 0;
    for v in &chosen {
        for g in &v.windows {
            max_width = max_width.max(window_name(g).chars().count());
        }
    }

    let mut block_strings = Vec::new();
    for v in chosen {
        let mut lines = Vec::new();

        let mark_color = match v.key.as_str() {
            "claude" => "38;2;217;119;87", // Coral
            "gemini" => "38;2;66;133;244", // Google Blue
            "codex" => "38;2;16;163;127",  // OpenAI Green
            "grok" => "38;2;255;255;255",  // White
            _ => "1",
        };

        let mut header = paint(mark_color, &format!("{} {}", v.mark, v.name), tty);
        if let Some(ref plan) = v.plan {
            header.push_str(&paint("2", &format!("  {}", plan), tty));
        }
        lines.push(header);

        if v.windows.is_empty() {
            let err = v.error.as_deref().unwrap_or("not logged in");
            lines.push(format!("  {}", err));
        } else {
            for g in &v.windows {
                let name = window_name(g);
                let name_padded = format!("{:<width$}", name, width = max_width);
                let bar = render_bar(g.percent);
                let colored_meter = paint(
                    meter_ansi_code(g.percent),
                    &format!("{:>3}%  {}", g.percent, bar),
                    tty,
                );
                let reset_text = g
                    .reset
                    .as_deref()
                    .and_then(reset_label)
                    .map(|label| paint("2", &format!("  resets {label}"), tty))
                    .unwrap_or_default();
                lines.push(format!("  {name_padded}  {colored_meter}{reset_text}"));
            }
            if let Some(ref err) = v.error {
                lines.push(format!("  {err}"));
            }
        }

        block_strings.push(lines.join("\n"));
    }

    block_strings.join("\n\n") + "\n"
}

/// Instant the quota window ends, when `reset` is an epoch or a UTC timestamp.
/// A human "Resets …" phrase is not an instant.
pub(crate) fn reset_instant(reset: &str) -> Option<SystemTime> {
    let reset = reset.trim();
    if reset.is_empty() {
        return None;
    }
    if reset.bytes().all(|byte| byte.is_ascii_digit()) {
        let secs: u64 = reset.parse().ok()?;
        // 2000-01-01 through 2100-01-01. Anything else is not a quota reset.
        if !(946_684_800..=4_102_444_800).contains(&secs) {
            return None;
        }
        return Some(UNIX_EPOCH + Duration::from_secs(secs));
    }
    let normalized = normalize_utc(reset)?;
    crate::job::parse_iso_time(&normalized)
}

fn normalize_utc(reset: &str) -> Option<String> {
    if let Some(plus) = reset.rfind('+') {
        let offset = &reset[plus..];
        if offset == "+00:00" || offset == "+0000" || offset == "+00" {
            return Some(format!("{}Z", reset[..plus].trim_end()));
        }
        return None;
    }
    if let Some(tpos) = reset.find('T') {
        let time = &reset[tpos + 1..];
        if let Some(idx) = time.rfind('-') {
            let offset = &time[idx..];
            if offset == "-00:00" || offset == "-0000" || offset == "-00" {
                return Some(format!("{}Z", reset[..tpos + 1 + idx].trim_end()));
            }
            if offset.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
        }
    }
    Some(reset.to_string())
}

pub(crate) fn reset_label(reset: &str) -> Option<String> {
    let trimmed = reset.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(instant) = reset_instant(trimmed) {
        return format_local(instant);
    }
    let phrase = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = phrase.chars().count();
    if !(6..64).contains(&chars)
        || !phrase.to_ascii_lowercase().contains("reset")
        || tokenish(&phrase)
    {
        return None;
    }
    // The bar already says "resets", so the stored "Resets …" word is not repeated.
    let shown = phrase
        .strip_prefix("Resets")
        .or_else(|| phrase.strip_prefix("resets"))
        .unwrap_or(phrase.as_str())
        .trim();
    if shown.is_empty() {
        return None;
    }
    Some(shown.to_string())
}

fn tokenish(text: &str) -> bool {
    let mut run = 0;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            run += 1;
            if run >= 24 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn format_local(instant: SystemTime) -> Option<String> {
    let unix = instant.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let Ok(secs) = libc::time_t::try_from(unix) else {
        return None;
    };
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // localtime_r writes only the tm we own and does not keep the pointer.
    let ptr = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ptr.is_null() {
        return None;
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Ok(mon) = usize::try_from(tm.tm_mon) else {
        return None;
    };
    if mon >= MONTHS.len()
        || !(1..=31).contains(&tm.tm_mday)
        || !(0..=23).contains(&tm.tm_hour)
        || !(0..=59).contains(&tm.tm_min)
    {
        return None;
    }
    Some(format!(
        "{} {} {:02}:{:02}",
        MONTHS[mon], tm.tm_mday, tm.tm_hour, tm.tm_min
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_bar_bounds_and_eighths() {
        // 0% -> all empty
        assert_eq!(render_bar(0), "░".repeat(20));
        // 100% -> all filled
        assert_eq!(render_bar(100), "█".repeat(20));

        // 5% -> exactly 1 block
        let bar_5 = render_bar(5);
        assert_eq!(bar_5, "█".to_string() + &"░".repeat(19));

        // 12% -> 2.4 blocks -> 2 whole + 3/8 partial
        let bar_12 = render_bar(12);
        assert_eq!(bar_12.chars().count(), 20);
        assert!(bar_12.starts_with("██▍"));
    }

    #[test]
    fn render_blocks_sorts_by_least_used_window() {
        let grok = ProviderView {
            key: "grok".to_string(),
            name: "grok".to_string(),
            mark: "✕".to_string(),
            plan: None,
            windows: vec![Gauge {
                id: "grok:weekly".to_string(),
                label: "Grok · week".to_string(),
                percent: 12,
                reset: None,
                window_secs: None,
            }],
            error: None,
        };
        let codex = ProviderView {
            key: "codex".to_string(),
            name: "codex".to_string(),
            mark: "⚛".to_string(),
            plan: Some("Pro".to_string()),
            windows: vec![Gauge {
                id: "codex:week".to_string(),
                label: "Codex · week".to_string(),
                percent: 100,
                reset: None,
                window_secs: None,
            }],
            error: None,
        };

        // Grok (12%) should precede Codex (100%)
        let out = render_blocks(&[codex, grok], false);
        let grok_idx = out.find("✕ grok").unwrap();
        let codex_idx = out.find("⚛ codex").unwrap();
        assert!(grok_idx < codex_idx);
        assert!(out.contains("12%  ██▍░░░░░░░░░░░░░░░░░"));
        assert!(out.contains("100%  ████████████████████"));
        assert!(!out.contains("resets"));
    }

    #[test]
    fn window_name_normalization_and_prefixes() {
        let g1 = Gauge {
            id: "claude:session".to_string(),
            label: "Claude · 5 h".to_string(),
            percent: 90,
            reset: None,
            window_secs: None,
        };
        assert_eq!(window_name(&g1), "5h");

        let g2 = Gauge {
            id: "claude:weekly_all".to_string(),
            label: "Claude · týždeň".to_string(),
            percent: 20,
            reset: None,
            window_secs: None,
        };
        assert_eq!(window_name(&g2), "week");

        let g3 = Gauge {
            id: "codex:primary_window".to_string(),
            label: "Codex · týždeň".to_string(),
            percent: 100,
            reset: None,
            window_secs: None,
        };
        assert_eq!(window_name(&g3), "week");

        let g4 = Gauge {
            id: "antigravity:gemini-5h".to_string(),
            label: "Gemini · 5 h".to_string(),
            percent: 40,
            reset: None,
            window_secs: None,
        };
        assert_eq!(window_name(&g4), "gemini 5h");

        let g5 = Gauge {
            id: "antigravity:gemini-argon-wtu-weekly".to_string(),
            label: "Gemini 4 Argon · týždeň".to_string(),
            percent: 70,
            reset: None,
            window_secs: None,
        };
        assert_eq!(window_name(&g5), "Gemini 4 Argon week");
    }
}
