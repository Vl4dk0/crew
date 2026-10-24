//! Update the harness CLIs with each CLI's own updater.
//!
//! A running crew agent is left alone: replacing its binary mid-run can
//! break that process. A CLI that is not installed is reported and skipped.
//! crew itself is not updated; this binary is not one of those updaters.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use crate::error::Error;
use crate::load;
use crate::providers;

/// An updater that is still downloading must not hold the command open.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `--version` is a local print. A hang here is a stuck CLI, not a slow net.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

struct Harness {
    /// argv0 of that CLI, and the name printed on the report line.
    bin: &'static str,
    /// Key `crew load` uses. agy is the gemini provider.
    provider: &'static str,
}

/// Spec order: claude, codex, grok, then gemini's `agy` binary.
const HARNESSES: [Harness; 4] = [
    Harness {
        bin: "claude",
        provider: "claude",
    },
    Harness {
        bin: "codex",
        provider: "codex",
    },
    Harness {
        bin: "grok",
        provider: "grok",
    },
    Harness {
        bin: "agy",
        provider: "gemini",
    },
];

pub fn run() -> Result<i32, Error> {
    let mut failed = false;
    for harness in HARNESSES {
        if !binary_on_path(harness.bin) {
            println!("{}: not found", harness.bin);
            continue;
        }
        // Count immediately before the updater. An agent that started during
        // the previous CLI's download still has to keep its own binary.
        let running = load::running_agents()
            .get(harness.provider)
            .map(|pids| pids.len())
            .unwrap_or(0);
        if running > 0 {
            println!("{} skipped: {running} running agent(s)", harness.bin);
            continue;
        }
        let before = match read_version(harness.bin) {
            Ok(version) => version,
            Err(reason) => {
                println!("{} update failed: {reason}", harness.bin);
                failed = true;
                continue;
            }
        };
        if let Err(reason) = run_update(harness.bin) {
            println!("{} update failed: {reason}", harness.bin);
            failed = true;
            continue;
        }
        let after = match read_version(harness.bin) {
            Ok(version) => version,
            Err(reason) => {
                println!("{} update failed: {reason}", harness.bin);
                failed = true;
                continue;
            }
        };
        if before == after {
            println!("{} {before} (up to date)", harness.bin);
        } else {
            println!("{} {before} -> {after}", harness.bin);
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn read_version(bin: &str) -> Result<String, String> {
    let captured =
        providers::run_bounded(&[bin.to_string(), "--version".to_string()], VERSION_TIMEOUT)
            .map_err(|err| err.to_string())?;
    if captured.timed_out {
        return Err("timed out".to_string());
    }
    if captured.code != 0 {
        return Err(short_reason(
            &captured.stderr,
            &captured.stdout,
            captured.code,
        ));
    }
    version_token(&captured.stdout)
        .or_else(|| version_token(&captured.stderr))
        .map(str::to_string)
        .ok_or_else(|| "no version in --version output".to_string())
}

fn run_update(bin: &str) -> Result<(), String> {
    let captured = providers::run_bounded(&[bin.to_string(), "update".to_string()], UPDATE_TIMEOUT)
        .map_err(|err| err.to_string())?;
    if captured.timed_out {
        return Err("timed out".to_string());
    }
    if captured.code != 0 {
        return Err(short_reason(
            &captured.stderr,
            &captured.stdout,
            captured.code,
        ));
    }
    Ok(())
}

/// First `major.minor.patch` token. The banners differ (`2.1.306 (Claude Code)`,
/// `codex-cli 0.164.0`, `grok 1.0.62 (...) [stable]`, `1.4.7`); the token does not.
fn version_token(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|token| is_version_token(token))
}

fn is_version_token(token: &str) -> bool {
    let mut count = 0usize;
    for part in token.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count >= 3
}

fn short_reason(stderr: &str, stdout: &str, code: i32) -> String {
    let stderr = strip_ansi(stderr);
    let stdout = strip_ansi(stdout);
    last_line(&stderr)
        .or_else(|| last_line(&stdout))
        .map(|line| clip(&line, 160))
        .unwrap_or_else(|| format!("exit {code}"))
}

fn last_line(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            None
        } else {
            Some(collapsed)
        }
    })
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        fs::metadata(&candidate).is_ok_and(|meta| meta.is_file() && is_executable(&meta))
    })
}

fn is_executable(meta: &fs::Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_token_from_cli_banners() {
        assert_eq!(version_token("2.1.306 (Claude Code)"), Some("2.1.306"));
        assert_eq!(version_token("codex-cli 0.164.0"), Some("0.164.0"));
        assert_eq!(version_token("grok 1.0.62 (...) [stable]"), Some("1.0.62"));
        assert_eq!(version_token("1.4.7"), Some("1.4.7"));
    }
}
