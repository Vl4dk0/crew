//! Antigravity (`agy`) headless runner.
//!
//! `agy -p` drops stdout off a terminal, so the child is attached to a PTY.
//! The parent never writes the prompt there. A headless tool denial exits 0;
//! the em-dash sentence is what turns that into 125.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{self, KillStyle, Report, StdioKind, map_effort, take_prompt};
use super::pty::strip_ansi;

const DENIAL_NEEDLE: &str = "no output produced \u{2014} a tool required the \"";

pub fn run(invocation: &Invocation) -> Result<i32, Error> {
    let resolved = tier::resolve(
        invocation.provider,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let prompt = take_prompt(invocation)?.ok_or_else(|| {
        Error::new(
            2,
            "empty prompt (pass a prompt argument or pipe one via stdin)",
        )
    })?;
    let (argv, note, sent_model) = gemini_argv(
        invocation,
        resolved.model.as_deref(),
        &resolved.effort,
        &prompt,
    );
    proc::write_effort_note(&note);
    let label = sent_model
        .clone()
        .or_else(|| invocation.agent.clone())
        .unwrap_or_else(|| "gemini".to_string());
    let home = cli_home();
    let track_files = invocation.print_id
        && invocation
            .resume
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_none();
    let before = if track_files {
        conv_snapshot(&home.join("conversations"))
    } else {
        HashMap::new()
    };
    let started = Instant::now();
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let captured = match proc::launch_or_note(
        proc::capture(
            &argv,
            invocation.cwd.as_deref(),
            timeout,
            StdioKind::Pty,
            KillStyle::TermThenKill,
            false,
        ),
        started,
        &label,
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    // Our own deadline kills the group and drops the partial PTY transcript.
    // A child that exits by itself with "timeout waiting for response" still
    // goes through present_gemini, which keeps that message.
    let mut presented = if captured.timed_out {
        let (_, before) = judge_gemini("", 124, invocation.read_only);
        let before = if before.is_empty() {
            format!(
                "[crew] timed out after {}s\n",
                proc::format_secs(invocation.timeout_secs)
            )
        } else {
            before
        };
        proc::Report {
            code: 124,
            before_stderr: before,
            stdout: String::new(),
            after_stderr: String::new(),
        }
    } else {
        present_gemini(&captured.stdout, captured.code, invocation.read_only)
    };
    if invocation.print_id {
        let resume = invocation.resume.as_deref().filter(|id| !id.is_empty());
        let id = if resume.is_some() {
            pick_session(resume, None, None)
        } else {
            let workdir = real_workdir(invocation.cwd.as_deref());
            let key = path_string(&workdir);
            let mapped = last_map_id(&home, &key);
            let newest = newest_changed(&before, &conv_snapshot(&home.join("conversations")));
            pick_session(None, mapped.as_deref(), newest.as_deref())
        };
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            presented.after_stderr = format!("SESSION_ID={id}\n");
        }
    }
    proc::emit_report(&presented);
    proc::note_long_run(started, &label);
    Ok(presented.code)
}

pub(super) fn gemini_argv(
    invocation: &Invocation,
    resolved_model: Option<&str>,
    resolved_effort: &str,
    prompt: &str,
) -> (Vec<String>, Option<String>, Option<String>) {
    let (model, effort_flag, note) = gemini_model(
        invocation.model.as_deref(),
        invocation.effort.is_some(),
        invocation.agent.is_some(),
        resolved_model,
        resolved_effort,
    );
    let prompt = proc::read_only_prompt(invocation, prompt);
    let mut argv = vec![
        "agy".to_string(),
        "-p".to_string(),
        prompt,
        "--print-timeout".to_string(),
        print_timeout(invocation.timeout_secs),
    ];
    if let Some(model) = &model {
        proc::push_pair(&mut argv, "--model", model);
    }
    if let Some(effort) = &effort_flag {
        proc::push_pair(&mut argv, "--effort", effort);
    }
    if let Some(agent) = &invocation.agent {
        proc::push_pair(&mut argv, "--agent", agent);
    }
    if let Some(resume) = &invocation.resume {
        proc::push_pair(&mut argv, "--conversation", resume);
    }
    if invocation.continue_session {
        argv.push("-c".to_string());
    }
    if invocation.write {
        proc::push_pair(&mut argv, "--mode", "accept-edits");
    } else if !invocation.read_only {
        argv.push("--dangerously-skip-permissions".to_string());
    }
    if invocation.sandbox {
        argv.push("--sandbox".to_string());
    }
    for directory in &invocation.add_dir {
        let directory = directory.to_string_lossy();
        proc::push_pair(&mut argv, "--add-dir", &directory);
    }
    (argv, note, model)
}

fn gemini_model(
    user_model: Option<&str>,
    user_set_effort: bool,
    agent: bool,
    resolved_model: Option<&str>,
    resolved_effort: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    if agent && user_model.is_none() {
        return effort_only(user_set_effort, resolved_effort);
    }
    let Some(model) = user_model.or(resolved_model) else {
        return effort_only(user_set_effort, resolved_effort);
    };
    if model == "flash" || model == "argon" {
        let (wire, note) = map_effort(Provider::Gemini, None, resolved_effort);
        let slug = if model == "flash" {
            format!("gemini-3.8-flash-{wire}")
        } else {
            format!("gemini-4-argon-{wire}")
        };
        return (Some(slug), None, note);
    }
    // A user `-m` that already names the effort is sent unchanged. agy rejects
    // a second `--effort` on that slug.
    if user_model.is_some() && baked_base(model).is_some() {
        return (Some(model.to_string()), None, None);
    }
    if let Some(base) = baked_base(model) {
        if user_set_effort {
            let (wire, note) = map_effort(Provider::Gemini, Some(model), resolved_effort);
            return (Some(format!("{base}-{wire}")), None, note);
        }
        return (Some(model.to_string()), None, None);
    }
    if user_set_effort {
        let (wire, note) = map_effort(Provider::Gemini, Some(model), resolved_effort);
        return (Some(model.to_string()), Some(wire), note);
    }
    (Some(model.to_string()), None, None)
}

fn effort_only(
    user_set_effort: bool,
    resolved_effort: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    if user_set_effort {
        let (wire, note) = map_effort(Provider::Gemini, None, resolved_effort);
        (None, Some(wire), note)
    } else {
        (None, None, None)
    }
}

fn baked_base(model: &str) -> Option<&str> {
    for suffix in ["-low", "-medium", "-high"] {
        if let Some(base) = model.strip_suffix(suffix)
            && !base.is_empty()
        {
            return Some(base);
        }
    }
    None
}

fn print_timeout(secs: f64) -> String {
    let whole = if secs.is_finite() {
        secs.trunc().clamp(0.0, i64::MAX as f64) as i64
    } else {
        0
    };
    format!("{}s", (whole - 10).max(10))
}

pub(super) fn present_gemini(raw: &str, rc: i32, read_only: bool) -> Report {
    let clean = strip_ansi(raw).replace('\r', "");
    let clean = clean.trim().to_string();
    let (code, before) = judge_gemini(&clean, rc, read_only);
    Report {
        code,
        before_stderr: before,
        stdout: format!("{clean}\n"),
        after_stderr: String::new(),
    }
}

fn judge_gemini(clean: &str, rc: i32, read_only: bool) -> (i32, String) {
    if let Some(tool) = denial_tool(clean).filter(|_| rc == 0) {
        let note = format!(
            "[crew] tool '{tool}' auto-denied by headless permission checks (125). Allow it in ~/.gemini/antigravity-cli/settings.json under permissions.allow — e.g. \"read_file(*)\" or \"command(ls *)\" (args need the ' *' wildcard) — or re-run with --write for edits, or drop the mode flag for full autonomy.\n"
        );
        return (125, note);
    }
    if clean.contains("timeout waiting for response") && rc != 0 {
        let note = "[crew] agy timed out waiting for a response (124). Raise -t for genuinely long tasks; with --read-only, a tool blocked on approval can also cause this — extend permissions.allow, or pass --write for edits, or drop the mode flag for full autonomy.\n";
        return (124, note.to_string());
    }
    if rc == 124 && read_only {
        let note = "[crew] timed out (124). If agy needed a tool outside the permissions.allow list it may block on an approval prompt; extend the allowlist, or pass --write for edits, or drop the mode flag for full autonomy.\n";
        return (124, note.to_string());
    }
    (rc, String::new())
}

fn denial_tool(clean: &str) -> Option<&str> {
    let mut start = 0;
    while let Some(relative) = clean[start..].find(DENIAL_NEEDLE) {
        let name_at = start + relative + DENIAL_NEEDLE.len();
        let Some(end) = clean[name_at..].find('"') else {
            break;
        };
        let name = &clean[name_at..name_at + end];
        if !name.is_empty() && name.chars().all(|ch| ch == '_' || ch.is_alphanumeric()) {
            return Some(name);
        }
        start = name_at + end + 1;
        if start > clean.len() {
            break;
        }
    }
    None
}

pub(super) fn pick_session(
    resume: Option<&str>,
    mapped: Option<&str>,
    newest: Option<&str>,
) -> Option<String> {
    resume
        .filter(|id| !id.is_empty())
        .or(mapped.filter(|id| !id.is_empty()))
        .or(newest.filter(|id| !id.is_empty()))
        .map(str::to_string)
}

fn cli_home() -> PathBuf {
    if let Some(path) = std::env::var_os("ANTIGRAVITY_CLI_HOME") {
        return PathBuf::from(path);
    }
    proc::expand_user("~/.gemini/antigravity-cli")
}

fn last_map_id(home: &Path, workdir: &str) -> Option<String> {
    let text = fs::read_to_string(home.join("cache").join("last_conversations.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get(workdir).and_then(Value::as_str).and_then(|id| {
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    })
}

fn conv_snapshot(dir: &Path) -> HashMap<String, SystemTime> {
    let mut found = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".pb") || name.ends_with(".db")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        found.insert(name.into_owned(), modified);
    }
    found
}

fn newest_changed(
    before: &HashMap<String, SystemTime>,
    after: &HashMap<String, SystemTime>,
) -> Option<String> {
    let mut changed: Vec<(&str, SystemTime)> = after
        .iter()
        .filter(|(name, modified)| before.get(*name) != Some(*modified))
        .map(|(name, modified)| (name.as_str(), *modified))
        .collect();
    changed.sort_by_key(|left| std::cmp::Reverse(left.1));
    let (name, _) = changed.first()?;
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn real_workdir(cwd: Option<&Path>) -> PathBuf {
    let path = match cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    fs::canonicalize(&path).unwrap_or(path)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::proc::{bare, flag, has};

    #[test]
    fn argv_bakes_effort_into_the_slug() {
        let invocation = bare(Provider::Gemini);
        let (argv, note, _) = gemini_argv(
            &invocation,
            Some("gemini-3.8-flash-medium"),
            "medium",
            "hello",
        );
        assert!(note.is_none());
        assert_eq!(
            argv,
            vec![
                "agy",
                "-p",
                "hello",
                "--print-timeout",
                "1790s",
                "--model",
                "gemini-3.8-flash-medium",
                "--dangerously-skip-permissions",
            ]
        );

        let mut invocation = bare(Provider::Gemini);
        invocation.timeout_secs = 15.0;
        invocation.model = Some("flash".to_string());
        invocation.effort = Some("max".to_string());
        let (argv, note, _) = gemini_argv(&invocation, Some("flash"), "max", "hello");
        assert_eq!(flag(&argv, "--model"), Some("gemini-3.8-flash-high"));
        assert_eq!(flag(&argv, "--print-timeout"), Some("10s"));
        assert!(!has(&argv, "--effort"));
        assert_eq!(note.as_deref(), Some("[crew] effort max -> high"));

        invocation.model = Some("argon".to_string());
        invocation.effort = None;
        let (argv, note, _) = gemini_argv(&invocation, Some("argon"), "medium", "hello");
        assert_eq!(flag(&argv, "--model"), Some("gemini-4-argon-medium"));
        assert!(note.is_none());
        assert!(!has(&argv, "--effort"));

        invocation.model = Some("gemini-3.8-flash-high".to_string());
        invocation.effort = Some("max".to_string());
        let (argv, note, _) =
            gemini_argv(&invocation, Some("gemini-3.8-flash-high"), "max", "hello");
        assert_eq!(flag(&argv, "--model"), Some("gemini-3.8-flash-high"));
        assert!(!has(&argv, "--effort"));
        assert!(note.is_none());

        invocation.model = None;
        invocation.effort = Some("high".to_string());
        let (argv, note, _) =
            gemini_argv(&invocation, Some("gemini-3.8-flash-low"), "high", "hello");
        assert_eq!(flag(&argv, "--model"), Some("gemini-3.8-flash-high"));
        assert!(!has(&argv, "--effort"));
        assert!(note.is_none());

        invocation.model = Some("gemini-3.8-flash".to_string());
        invocation.effort = Some("high".to_string());
        let (argv, _, _) = gemini_argv(&invocation, Some("gemini-3.8-flash"), "high", "hello");
        assert_eq!(flag(&argv, "--model"), Some("gemini-3.8-flash"));
        assert_eq!(flag(&argv, "--effort"), Some("high"));

        invocation.model = None;
        invocation.effort = None;
        invocation.agent = Some("reviewer".to_string());
        let (argv, _, _) = gemini_argv(&invocation, Some("gemini-3.8-flash-high"), "high", "hello");
        assert!(!has(&argv, "--model"));
        assert!(!has(&argv, "--effort"));
        assert_eq!(flag(&argv, "--agent"), Some("reviewer"));

        invocation.agent = None;
        invocation.read_only = true;
        invocation.sandbox = true;
        invocation.add_dir = vec!["~/notes".into()];
        let (argv, _, _) = gemini_argv(
            &invocation,
            Some("gemini-3.8-flash-medium"),
            "medium",
            "hello",
        );
        assert!(!has(&argv, "--dangerously-skip-permissions"));
        assert!(!has(&argv, "--mode"));
        assert!(has(&argv, "--sandbox"));
        assert_eq!(flag(&argv, "--add-dir"), Some("~/notes"));
        assert_eq!(argv[2], format!("hello\n\n{}", proc::READ_ONLY_LINE));

        invocation.raw = true;
        let (argv, _, _) = gemini_argv(
            &invocation,
            Some("gemini-3.8-flash-medium"),
            "medium",
            "hello",
        );
        assert_eq!(argv[2], format!("hello\n\n{}", proc::READ_ONLY_LINE));
        invocation.raw = false;

        invocation.read_only = false;
        invocation.write = true;
        let (argv, _, _) = gemini_argv(
            &invocation,
            Some("gemini-3.8-flash-medium"),
            "medium",
            "hello",
        );
        assert_eq!(flag(&argv, "--mode"), Some("accept-edits"));
    }

    #[test]
    fn denial_and_timeout_waiting() {
        let denied =
            "\u{1b}[31mno output produced \u{2014} a tool required the \"read_file\"\u{1b}[0m";
        let report = present_gemini(denied, 0, true);
        assert_eq!(report.code, 125);
        assert!(report.stdout.contains("read_file"));
        assert!(report.stdout.contains('\u{2014}'));
        assert!(!report.stdout.contains('\u{1b}'));
        assert!(report.before_stderr.contains("125"));

        let hyphenated = "no output produced \u{2014} a tool required the \"read-file\"";
        assert_eq!(present_gemini(hyphenated, 0, false).code, 0);
        let ascii_dash = "no output produced - a tool required the \"read_file\"";
        assert_eq!(present_gemini(ascii_dash, 0, false).code, 0);
        assert_eq!(present_gemini(denied, 1, false).code, 1);

        let waiting = "timeout waiting for response";
        assert_eq!(present_gemini(waiting, 1, false).code, 124);
        assert_eq!(present_gemini(waiting, 0, false).code, 0);
        assert_eq!(present_gemini("", 0, false).stdout, "\n");

        let timed = present_gemini("still working", 124, true);
        assert_eq!(timed.code, 124);
        assert!(timed.before_stderr.contains("permissions.allow"));
        assert!(timed.stdout.contains("still working"));
    }

    #[test]
    fn resume_id_is_not_replaced() {
        assert_eq!(
            pick_session(Some("abc"), Some("other"), Some("new")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            pick_session(Some(""), Some("other"), None).as_deref(),
            Some("other")
        );
        assert_eq!(
            pick_session(None, None, Some("new")).as_deref(),
            Some("new")
        );
    }
}
