//! Update the harness CLIs with each CLI's own updater, then crew itself.
//!
//! A running crew agent is left alone: replacing its binary mid-run can
//! break that process. A CLI that is not installed is reported and skipped.
//! crew is last, and only a git install moves. A path install is Vladko's
//! checkout, so it is left untouched. Cargo renames the new binary into
//! place, so a crew process that is already running keeps the one it loaded.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use crate::error::Error;
use crate::load;
use crate::providers;

/// An updater that is still downloading must not hold the command open.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `--version` and `cargo install --list` are local. A hang is a stuck CLI.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// `cargo install --git` compiles. A stuck build must not hold the command open.
const CREW_INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A private repo without credentials should fail here, not sit on a prompt.
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

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
    // After the harnesses. A failure there still tries crew; either one fails the run.
    if let Err(reason) = update_self() {
        println!("crew update failed: {reason}");
        failed = true;
    }
    Ok(if failed { 1 } else { 0 })
}

/// Where `cargo install` put this crew, from `cargo install --list`.
///
/// The list line is the only record we trust. A path is a checkout, not a
/// release to rebuild. A git URL is reinstalled when its commit moved.
#[derive(Debug, PartialEq, Eq)]
enum CrewInstall {
    Missing,
    Path(String),
    Git {
        url: String,
        branch: Option<String>,
        sha: String,
    },
    /// `--tag` or `--rev` is a pin. Moving it to HEAD would drop that pin.
    Pinned {
        kind: &'static str,
        value: String,
    },
    Other(String),
}

fn update_self() -> Result<(), String> {
    let list = read_install_list()?;
    match parse_install_list(&list) {
        CrewInstall::Missing => {
            println!("crew: not installed via cargo");
            Ok(())
        }
        CrewInstall::Path(path) => {
            println!("crew: installed from {path}, skipped");
            Ok(())
        }
        CrewInstall::Other(source) => {
            println!("crew: installed from {source}, skipped");
            Ok(())
        }
        CrewInstall::Pinned { kind, value } => {
            println!("crew: installed from git {kind} {value}, skipped");
            Ok(())
        }
        CrewInstall::Git { url, branch, sha } => install_if_moved(&url, branch.as_deref(), &sha),
    }
}

fn read_install_list() -> Result<String, String> {
    let captured = providers::run_bounded(
        &[
            "cargo".to_string(),
            "install".to_string(),
            "--list".to_string(),
        ],
        VERSION_TIMEOUT,
    )
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
    Ok(captured.stdout)
}

fn install_if_moved(url: &str, branch: Option<&str>, installed: &str) -> Result<(), String> {
    let remote = remote_sha(url, branch)?;
    if shas_match(installed, &remote) {
        println!("crew {} (up to date)", short_sha(installed));
        return Ok(());
    }
    // `--branch` only when the install recorded one. Without it, cargo would
    // build the default branch and a branch install would jump.
    let mut argv = vec![
        "cargo".to_string(),
        "install".to_string(),
        "--git".to_string(),
        url.to_string(),
    ];
    if let Some(branch) = branch {
        argv.push("--branch".to_string());
        argv.push(branch.to_string());
    }
    argv.push("--locked".to_string());
    argv.push("--force".to_string());
    let captured =
        providers::run_bounded(&argv, CREW_INSTALL_TIMEOUT).map_err(|err| err.to_string())?;
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
    println!("crew {} -> {}", short_sha(installed), short_sha(&remote));
    Ok(())
}

fn remote_sha(url: &str, branch: Option<&str>) -> Result<String, String> {
    let spec = match branch {
        Some(branch) => format!("refs/heads/{branch}"),
        None => "HEAD".to_string(),
    };
    let captured = providers::run_bounded(
        &[
            "git".to_string(),
            "ls-remote".to_string(),
            "--".to_string(),
            url.to_string(),
            spec,
        ],
        LS_REMOTE_TIMEOUT,
    )
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
    ls_remote_sha(&captured.stdout).ok_or_else(|| "ls-remote returned no sha".to_string())
}

fn ls_remote_sha(output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    let sha = line.split_whitespace().next()?;
    if is_hex_sha(sha) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Cargo's list shows at most 8 hex digits. The same commit matches that prefix.
fn shas_match(installed: &str, remote: &str) -> bool {
    let installed = installed.to_ascii_lowercase();
    let remote = remote.to_ascii_lowercase();
    !installed.is_empty() && (remote.starts_with(&installed) || installed.starts_with(&remote))
}

fn short_sha(sha: &str) -> &str {
    let n = sha.len().min(7);
    &sha[..n]
}

fn parse_install_list(list: &str) -> CrewInstall {
    for line in list.lines() {
        let line = line.trim_end();
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some(rest) = line.strip_prefix("crew ") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(':') else {
            continue;
        };
        let Some((_, source)) = rest.split_once(" (") else {
            return CrewInstall::Other("crates.io".to_string());
        };
        let Some(source) = source.strip_suffix(')') else {
            continue;
        };
        return classify_source(source);
    }
    CrewInstall::Missing
}

fn classify_source(source: &str) -> CrewInstall {
    if let Some(path) = path_source(source) {
        return CrewInstall::Path(path);
    }
    if let Some(install) = git_source(source) {
        return install;
    }
    CrewInstall::Other(source.to_string())
}

fn path_source(source: &str) -> Option<String> {
    let url = source.strip_prefix("path+").unwrap_or(source);
    if let Some(rest) = url.strip_prefix("file://") {
        // `file:///Users/vladko` -> `/Users/vladko`.
        return Some(rest.to_string());
    }
    if source.starts_with('/') {
        return Some(source.to_string());
    }
    None
}

fn git_source(source: &str) -> Option<CrewInstall> {
    let (base, sha) = source.rsplit_once('#')?;
    if !is_hex_sha(sha) {
        return None;
    }
    let (url, query) = match base.split_once('?') {
        Some((url, query)) => (url, Some(query)),
        None => (base, None),
    };
    if !looks_like_git_url(url) {
        return None;
    }
    if let Some(query) = query {
        // Same last-key-wins order cargo uses when it reads the URL.
        let mut branch = None;
        let mut pin: Option<(&'static str, String)> = None;
        for (key, value) in query_pairs(query) {
            match key.as_str() {
                "branch" | "ref" => {
                    pin = None;
                    branch = Some(value);
                }
                "tag" => {
                    branch = None;
                    pin = Some(("tag", value));
                }
                "rev" => {
                    branch = None;
                    pin = Some(("rev", value));
                }
                _ => {}
            }
        }
        if let Some((kind, value)) = pin {
            return Some(CrewInstall::Pinned { kind, value });
        }
        if let Some(branch) = branch {
            return Some(CrewInstall::Git {
                url: url.to_string(),
                branch: Some(branch),
                sha: sha.to_string(),
            });
        }
    }
    Some(CrewInstall::Git {
        url: url.to_string(),
        branch: None,
        sha: sha.to_string(),
    })
}

fn looks_like_git_url(url: &str) -> bool {
    url.contains("://") || url.starts_with("git@")
}

fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(high) = hex_value(bytes[index + 1])
            && let Some(low) = hex_value(bytes[index + 2])
        {
            out.push((high << 4) | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_hex_sha(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    #[test]
    fn install_list_line_is_path_or_git() {
        let path = "\
mdbook v0.4.43:
    mdbook
crew v0.1.0 (/Users/vladko/Projects/crew):
    crew
";
        assert_eq!(
            parse_install_list(path),
            CrewInstall::Path("/Users/vladko/Projects/crew".to_string())
        );

        let https = "crew v0.1.0 (https://github.com/Vl4dk0/crew#abcdef12):\n    crew\n";
        assert_eq!(
            parse_install_list(https),
            CrewInstall::Git {
                url: "https://github.com/Vl4dk0/crew".to_string(),
                branch: None,
                sha: "abcdef12".to_string(),
            }
        );

        let ssh = "crew v0.1.0 (ssh://git@github.com/Vl4dk0/crew#abcdef12):\n";
        assert_eq!(
            parse_install_list(ssh),
            CrewInstall::Git {
                url: "ssh://git@github.com/Vl4dk0/crew".to_string(),
                branch: None,
                sha: "abcdef12".to_string(),
            }
        );

        let branch =
            "crew v0.1.0 (https://github.com/Vl4dk0/crew?branch=feat%2Fupdate#abcdef12):\n";
        assert_eq!(
            parse_install_list(branch),
            CrewInstall::Git {
                url: "https://github.com/Vl4dk0/crew".to_string(),
                branch: Some("feat/update".to_string()),
                sha: "abcdef12".to_string(),
            }
        );
    }
}
