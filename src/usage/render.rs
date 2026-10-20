use serde::{Deserialize, Serialize};

pub const BAR_WIDTH: usize = 20;
const EIGHTH: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gauge {
    pub id: String,
    pub label: String,
    pub percent: u32,
    #[serde(default)]
    pub reset: Option<String>,
    #[serde(default)]
    pub stale: bool,
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

pub fn window_name(g: &Gauge) -> String {
    if let Some(pos) = g.label.find('·') {
        g.label[pos + '·'.len_utf8()..].trim().to_string()
    } else {
        g.label.trim().to_string()
    }
}

fn sort_key(view: &ProviderView) -> (u8, u32, String) {
    if view.windows.is_empty() {
        return (2, 0, view.name.clone());
    }
    let all_stale = view.windows.iter().all(|g| g.stale);
    let max_used = view.windows.iter().map(|g| g.percent).max().unwrap_or(0);
    (if all_stale { 1 } else { 0 }, max_used, view.name.clone())
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
            let err = v.error.as_deref().unwrap_or("not signed in");
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
                let stale_text = if g.stale {
                    paint("2", "  stale", tty)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "  {}  {}{}",
                    name_padded, colored_meter, stale_text
                ));
            }
            if let Some(ref err) = v.error {
                lines.push(format!("  {}", err));
            }
        }

        block_strings.push(lines.join("\n"));
    }

    block_strings.join("\n\n") + "\n"
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
                stale: false,
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
                stale: false,
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
    }
}
