//! What this machine's local CLI sessions would have cost at API prices.
//!
//! Claude and Codex write per-response token counts. Grok and Antigravity do
//! not, so they get no figure. The sum is every local session in the quota
//! window, not only crew runs.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::plan::format_usd;
use super::prices::{self, Book};
use super::render::{Gauge, reset_instant};

struct Tally {
    usd: f64,
    unpriced: u64,
    priced: u64,
}

impl Tally {
    fn empty() -> Self {
        Self {
            usd: 0.0,
            unpriced: 0,
            priced: 0,
        }
    }
}

struct Parts {
    key: Option<String>,
    model: Option<String>,
    fresh: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

enum CodexEvent {
    Model(String),
    Usage {
        at: Option<SystemTime>,
        fresh: u64,
        cache_read: u64,
        cache_write: u64,
        output: u64,
    },
}

pub(crate) fn line(
    provider: &str,
    windows: &[Gauge],
    monthly_usd: Option<f64>,
    book: &Book,
) -> Option<String> {
    if provider != "claude" && provider != "codex" {
        return None;
    }
    let now = SystemTime::now();
    let (start, end, label) = choose_window(windows, now);
    let tally = match provider {
        "claude" => scan_claude(start, end, book),
        _ => scan_codex(start, end, book),
    };
    render_tally(&tally, label, monthly_usd, book.is_empty())
}

fn choose_window(windows: &[Gauge], now: SystemTime) -> (SystemTime, SystemTime, &'static str) {
    let mut best: Option<(u64, SystemTime, SystemTime)> = None;
    for gauge in windows {
        let Some(secs) = gauge.window_secs.filter(|secs| *secs > 0) else {
            continue;
        };
        let Some(reset) = gauge.reset.as_deref().and_then(reset_instant) else {
            continue;
        };
        if let Ok(past) = now.duration_since(reset) {
            if past > Duration::from_secs(60 * 60) {
                continue;
            }
        }
        let Some(start) = reset.checked_sub(Duration::from_secs(secs)) else {
            continue;
        };
        let replace = best.as_ref().is_none_or(|(old, _, _)| secs > *old);
        if replace {
            best = Some((secs, start, reset));
        }
    }
    if let Some((secs, start, end)) = best {
        return (start, end, period_label(secs));
    }
    let start = now
        .checked_sub(Duration::from_secs(7 * 24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    (start, now, "last 7d")
}

fn period_label(secs: u64) -> &'static str {
    let days = secs as f64 / 86_400.0;
    if (6.0..=8.0).contains(&days) {
        "this week"
    } else if (27.0..=32.0).contains(&days) {
        "this month"
    } else if (4 * 3600..=6 * 3600).contains(&secs) {
        "this 5h"
    } else {
        "this period"
    }
}

fn render_tally(
    tally: &Tally,
    label: &str,
    monthly: Option<f64>,
    book_empty: bool,
) -> Option<String> {
    if tally.priced == 0 && tally.unpriced == 0 {
        return None;
    }
    let plan = monthly
        .map(|usd| format!(" · plan {}/mo", format_usd(usd)))
        .unwrap_or_default();
    if book_empty {
        return Some(format!("api no price cache {label}{plan}"));
    }
    if tally.priced == 0 {
        return Some(format!("api price unknown {label}{plan}"));
    }
    let money = format_usd(tally.usd);
    if tally.unpriced > 0 {
        Some(format!(
            "api {money} {label}, unpriced tokens left out{plan}"
        ))
    } else {
        Some(format!("api {money} {label}{plan}"))
    }
}

fn add_parts(
    tally: &mut Tally,
    book: &Book,
    model: Option<&str>,
    fresh: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
) {
    let rate = model.and_then(|model| book.rate_for(model));
    let (usd, unpriced, priced) = prices::price_parts(rate, fresh, cache_read, cache_write, output);
    tally.usd += usd;
    tally.unpriced += unpriced;
    tally.priced += priced;
}

fn scan_claude(start: SystemTime, end: SystemTime, book: &Book) -> Tally {
    let mut tally = Tally::empty();
    let Some(root) = home_join(".claude/projects") else {
        return tally;
    };
    let mut named: HashMap<String, Parts> = HashMap::new();
    let mut anon = Vec::new();
    for path in session_files(&root, start) {
        let _ = for_each_line(&path, |line| {
            if !contains(line, b"\"type\":\"assistant\"")
                && !contains(line, b"\"type\": \"assistant\"")
            {
                return;
            }
            if !contains(line, b"usage") {
                return;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                return;
            };
            let Some(parts) = claude_parts(text, start, end) else {
                return;
            };
            if let Some(key) = parts.key.clone() {
                named.insert(key, parts);
            } else {
                anon.push(parts);
            }
        });
    }
    for parts in named.into_values().chain(anon) {
        add_parts(
            &mut tally,
            book,
            parts.model.as_deref(),
            parts.fresh,
            parts.cache_read,
            parts.cache_write,
            parts.output,
        );
    }
    tally
}

fn scan_codex(start: SystemTime, end: SystemTime, book: &Book) -> Tally {
    let mut tally = Tally::empty();
    let Some(root) = home_join(".codex/sessions") else {
        return tally;
    };
    for path in session_files(&root, start) {
        let mut model = None;
        let _ = for_each_line(&path, |line| {
            if !contains(line, b"token_usage_record") && !contains(line, b"turn_context") {
                return;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                return;
            };
            match parse_codex_line(text) {
                Some(CodexEvent::Model(name)) => model = Some(name),
                Some(CodexEvent::Usage {
                    at,
                    fresh,
                    cache_read,
                    cache_write,
                    output,
                }) => {
                    let Some(at) = at else {
                        return;
                    };
                    if at < start || at >= end {
                        return;
                    }
                    add_parts(
                        &mut tally,
                        book,
                        model.as_deref(),
                        fresh,
                        cache_read,
                        cache_write,
                        output,
                    );
                }
                None => {}
            }
        });
    }
    tally
}

fn home_join(rel: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(rel))
}

const SKIP_DIRS: &[&str] = &["node_modules", "plugins", "file-history", "worktrees"];

fn session_files(root: &Path, start: SystemTime) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_logs(root, start, &mut files);
    files.sort();
    files
}

fn walk_logs(dir: &Path, start: SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_logs(&path, start, out);
            continue;
        }
        if !is_session_log(&path) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if modified >= start {
            out.push(path);
        }
    }
}

fn is_session_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
}

fn for_each_line(path: &Path, mut on_line: impl FnMut(&[u8])) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name.ends_with(".jsonl.zst") {
        let Ok(mut child) = Command::new("zstd")
            .arg("-dc")
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(stdout) = child.stdout.take() {
            read_lines(stdout, &mut on_line);
        }
        let _ = child.wait();
        return true;
    }
    let Ok(file) = File::open(path) else {
        return true;
    };
    read_lines(file, &mut on_line);
    true
}

fn read_lines(reader: impl Read, on_line: &mut impl FnMut(&[u8])) {
    let mut reader = BufReader::with_capacity(1 << 20, reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => on_line(&buf),
            Err(_) => break,
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let mut index = 0;
    let last = hay.len() - needle.len();
    while index <= last {
        let Some(found) = hay[index..].iter().position(|&byte| byte == needle[0]) else {
            return false;
        };
        index += found;
        if hay[index..].starts_with(needle) {
            return true;
        }
        index += 1;
    }
    false
}

fn claude_parts(text: &str, start: SystemTime, end: SystemTime) -> Option<Parts> {
    let line: ClaudeLine = serde_json::from_str(text).ok()?;
    if line.kind.as_deref() != Some("assistant") {
        return None;
    }
    let at = line.timestamp.as_deref().and_then(reset_instant)?;
    if at < start || at >= end {
        return None;
    }
    let message = line.message?;
    let usage = message.usage?;
    let fresh = usage.input_tokens.unwrap_or(0);
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    if fresh == 0 && cache_read == 0 && cache_write == 0 && output == 0 {
        return None;
    }
    let key = line
        .request_id
        .filter(|id| !id.is_empty())
        .or(line.uuid.filter(|id| !id.is_empty()));
    Some(Parts {
        key,
        model: message.model.filter(|model| !model.is_empty()),
        fresh,
        cache_read,
        cache_write,
        output,
    })
}

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, rename = "requestId")]
    request_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

fn parse_codex_line(text: &str) -> Option<CodexEvent> {
    let line: CodexLine = serde_json::from_str(text).ok()?;
    match line.kind.as_deref() {
        Some("turn_context") => {
            let model = line.payload?.model?;
            if model.is_empty() {
                None
            } else {
                Some(CodexEvent::Model(model))
            }
        }
        Some("token_usage_record") => {
            let usage = line.payload?.usage?;
            let (fresh, cache_read, cache_write, output) = codex_parts(&usage)?;
            let at = line.timestamp.as_deref().and_then(reset_instant);
            Some(CodexEvent::Usage {
                at,
                fresh,
                cache_read,
                cache_write,
                output,
            })
        }
        _ => None,
    }
}

fn codex_parts(usage: &CodexUsage) -> Option<(u64, u64, u64, u64)> {
    let input = usage.input_tokens.unwrap_or(0);
    let cached = usage.cached_input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_input_tokens.unwrap_or(0);
    if cached > input {
        return None;
    }
    let fresh = input - cached;
    let extra_write = match usage.total_tokens {
        Some(total) if total == input.saturating_add(output) => 0,
        Some(total)
            if cache_write > 0
                && total == input.saturating_add(output).saturating_add(cache_write) =>
        {
            cache_write
        }
        Some(_) if cache_write > 0 => return None,
        _ => 0,
    };
    if fresh == 0 && cached == 0 && extra_write == 0 && output == 0 {
        return None;
    }
    Some((fresh, cached, extra_write, output))
}

#[derive(Deserialize)]
struct CodexLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<CodexUsage>,
}

#[derive(Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Folded {
        model: Option<String>,
        fresh: u64,
        cache_read: u64,
        cache_write: u64,
        output: u64,
    }

    fn fold_codex_lines(lines: &[&str]) -> Vec<Folded> {
        let mut model = None;
        let mut folded = Vec::new();
        for line in lines {
            match parse_codex_line(line) {
                Some(CodexEvent::Model(name)) => model = Some(name),
                Some(CodexEvent::Usage {
                    fresh,
                    cache_read,
                    cache_write,
                    output,
                    ..
                }) => folded.push(Folded {
                    model: model.clone(),
                    fresh,
                    cache_read,
                    cache_write,
                    output,
                }),
                None => {}
            }
        }
        folded
    }

    #[test]
    fn codex_usage_line_ignores_sibling_totals() {
        let lines = [
            r#"{"timestamp":"2026-10-21T00:00:00Z","type":"turn_context","payload":{"model":"gpt-6-astra"}}"#,
            r#"{"timestamp":"2026-10-21T00:00:01Z","type":"token_usage_record","payload":{"usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":10,"total_tokens":1010},"turn_token_usage":{"input_tokens":99999,"cached_input_tokens":99999,"output_tokens":99999},"thread_token_usage":{"input_tokens":99999,"cached_input_tokens":0,"output_tokens":99999}}}"#,
        ];
        let folded = fold_codex_lines(&lines);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(folded[0].fresh, 600);
        assert_eq!(folded[0].cache_read, 400);
        assert_eq!(folded[0].cache_write, 0);
        assert_eq!(folded[0].output, 10);
    }
}
