//! Codex headless runner.
//!
//! User-facing `max` is `ultra` on the wire, except `gpt-6-luna`, which has no
//! `ultra`. Any `turn.failed` fails the run, including one followed by
//! `turn.completed`. The deadline SIGKILLs the process group immediately.

use std::path::Path;

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{
    self, KillStyle, Report, StdioKind, USAGE_LIMIT_EXIT, format_secs, looks_like_usage_limit,
    map_effort, public_code, take_prompt, usage_limit_line,
};

pub fn run(invocation: &Invocation) -> Result<i32, Error> {
    if scope_count(invocation) > 1 {
        return Err(Error::new(
            2,
            "pass only one of --uncommitted, --base, --commit",
        ));
    }
    let resolved = tier::resolve(
        invocation.provider,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let model = resolved
        .model
        .ok_or_else(|| Error::new(2, "codex needs a model"))?;
    let prompt = take_prompt(invocation)?;
    let (argv, note) = codex_argv(invocation, &model, &resolved.effort, prompt.as_deref());
    proc::write_effort_note(&note);
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let captured = match proc::launch_or_note(proc::capture(
        &argv,
        invocation.cwd.as_deref(),
        timeout,
        StdioKind::Piped,
        KillStyle::ImmediateKill,
        true,
    )) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    let parsed = parse_events(&captured.stdout);
    let mut before = String::new();
    for line in &parsed.notices {
        before.push_str(&format!("[crew] {line}\n"));
    }
    if let Some(id) = parsed.thread_id.as_deref().filter(|id| !id.is_empty()) {
        before.push_str(&format!("SESSION_ID={id}\n"));
    }
    let mut stdout = String::new();
    if invocation.raw {
        stdout.push_str(&captured.stdout);
    } else if !parsed.message.is_empty() {
        stdout.push_str(parsed.message.trim_end());
        stdout.push('\n');
    }
    let mut after = String::new();
    if !captured.stderr.is_empty() {
        after.push_str(&captured.stderr);
    }
    let mut code = select_codex_exit(captured.code, parsed.success, captured.timed_out);
    if let Some(message) = parsed
        .usage_limit
        .as_deref()
        .filter(|_| !captured.timed_out && !parsed.success)
    {
        code = USAGE_LIMIT_EXIT;
        after.push_str(&usage_limit_line("codex", message));
    } else if captured.timed_out {
        after.push_str(&format!(
            "[crew] timed out after {}s\n",
            format_secs(invocation.timeout_secs)
        ));
    } else if code == 125 {
        after.push_str("[crew] failed or incomplete turn (missing completion or final answer)\n");
    }
    proc::emit_report(&Report {
        code,
        before_stderr: before,
        stdout,
        after_stderr: after,
    });
    Ok(code)
}

fn scope_count(invocation: &Invocation) -> usize {
    usize::from(invocation.uncommitted)
        + usize::from(invocation.base.is_some())
        + usize::from(invocation.commit.is_some())
}

pub(super) fn codex_argv(
    invocation: &Invocation,
    model: &str,
    effort: &str,
    prompt: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let (wire, note) = map_effort(Provider::Codex, Some(model), effort);
    let mut argv = vec![
        "codex".to_string(),
        "exec".to_string(),
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
    ];
    argv.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    for directory in &invocation.add_dir {
        let resolved = proc::resolve_no_tilde(directory);
        let resolved = resolved.to_string_lossy();
        proc::push_pair(&mut argv, "--add-dir", &resolved);
    }
    if invocation.resume.is_some() || invocation.continue_session {
        argv.push("resume".to_string());
    } else if invocation.review {
        argv.push("review".to_string());
    }
    argv.push("--json".to_string());
    argv.push("--skip-git-repo-check".to_string());
    proc::push_pair(&mut argv, "-m", model);
    proc::push_pair(
        &mut argv,
        "-c",
        &format!("model_reasoning_effort=\"{wire}\""),
    );
    if let Some(schema) = &invocation.schema {
        let resolved = proc::resolve_no_tilde(Path::new(schema));
        let resolved = resolved.to_string_lossy();
        proc::push_pair(&mut argv, "--output-schema", &resolved);
    }
    if invocation.continue_session {
        argv.push("--last".to_string());
    } else if let Some(resume) = &invocation.resume {
        argv.push(resume.clone());
    }
    if invocation.uncommitted {
        argv.push("--uncommitted".to_string());
    } else if let Some(base) = &invocation.base {
        proc::push_pair(&mut argv, "--base", base);
    } else if let Some(commit) = &invocation.commit {
        proc::push_pair(&mut argv, "--commit", commit);
    }
    if let Some(prompt) = prompt {
        argv.push("--".to_string());
        argv.push(prompt.to_string());
    }
    (argv, note)
}

pub(super) struct CodexParse {
    message: String,
    thread_id: Option<String>,
    success: bool,
    notices: Vec<String>,
    usage_limit: Option<String>,
}

/// The message of an `error` or `turn.failed` event when it is a usage-limit
/// stop. Codex tags it `usage_limit_exceeded`; the text match covers a stream
/// that only carries the message.
fn limit_message(event: &Value) -> Option<String> {
    let error = event.get("error").unwrap_or(event);
    let message = match error {
        Value::String(text) => text.as_str(),
        _ => error.get("message").and_then(Value::as_str).unwrap_or(""),
    };
    let tagged =
        error.get("codex_error_info").and_then(Value::as_str) == Some("usage_limit_exceeded");
    (tagged || looks_like_usage_limit(message)).then(|| message.to_string())
}

pub(super) fn parse_events(stdout: &str) -> CodexParse {
    let mut message = String::new();
    let mut thread_id = None;
    let mut completed = false;
    let mut failed = false;
    let mut notices = Vec::new();
    let mut usage_limit = None;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !event.is_object() {
            continue;
        }
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("error" | "turn.failed")
        ) && let Some(message) = limit_message(&event)
        {
            usage_limit = Some(message);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                thread_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
            Some("turn.completed") => completed = true,
            Some("turn.failed") => {
                failed = true;
                notices.push(line.to_string());
            }
            Some("error") => notices.push(line.to_string()),
            Some("item.completed") => {
                if let Some(item) = event.get("item").and_then(Value::as_object)
                    && item.get("type").and_then(Value::as_str) == Some("agent_message")
                {
                    message = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
            }
            _ => {}
        }
    }
    let success = completed && !failed && !message.trim().is_empty();
    CodexParse {
        message,
        thread_id,
        success,
        notices,
        usage_limit,
    }
}

pub(super) fn select_codex_exit(child: i32, success: bool, timed_out: bool) -> i32 {
    if timed_out {
        return 124;
    }
    let child = public_code(child);
    if child != 0 {
        return child;
    }
    if success { 0 } else { 125 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::proc::{LAUNCH_EXIT, bare, codex_scope, flag, has};

    #[test]
    fn turn_failed_is_not_saved_by_a_later_completion() {
        let stdout = "\
{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n\
{\"type\":\"turn.completed\"}\n\
{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"answer\"}}\n\
{\"type\":\"turn.failed\",\"error\":\"nope\"}\n\
{\"type\":\"turn.completed\"}\n\
{\"type\":\"thread.started\",\"thread_id\":\"t2\"}\n";
        let parsed = parse_events(stdout);
        assert!(!parsed.success);
        assert_eq!(parsed.thread_id.as_deref(), Some("t2"));
        assert_eq!(parsed.message, "answer");
        assert_eq!(parsed.notices.len(), 1);
        assert_eq!(select_codex_exit(0, parsed.success, false), 125);

        let stdout = "\
{\"type\":\"error\",\"message\":\"warn\"}\n\
{\"type\":\"turn.completed\"}\n\
{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}\n";
        let parsed = parse_events(stdout);
        assert!(parsed.success);
        assert_eq!(parsed.notices.len(), 1);
        assert_eq!(select_codex_exit(0, parsed.success, false), 0);
    }

    #[test]
    fn codex_usage_limit_event_is_detected() {
        let stdout = "{\"type\":\"error\",\"message\":\"You've hit your usage limit. Try again at 9:48 AM.\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"You've hit your usage limit. Try again at 9:48 AM.\"}}\n";
        let parsed = parse_events(stdout);
        assert!(!parsed.success);
        assert!(parsed.usage_limit.unwrap().contains("9:48 AM"));
        let warn = parse_events("{\"type\":\"error\",\"message\":\"warn\"}\n");
        assert!(warn.usage_limit.is_none());
    }

    #[test]
    fn exit_selection_covers_124_125_and_127() {
        assert_eq!(select_codex_exit(0, true, true), 124);
        assert_eq!(select_codex_exit(0, false, false), 125);
        assert_eq!(LAUNCH_EXIT, 127);
        assert_eq!(select_codex_exit(4, false, false), 4);
        assert_eq!(select_codex_exit(-9, false, false), 137);
        assert_eq!(select_codex_exit(-15, true, false), 143);
        assert_eq!(select_codex_exit(0, true, false), 0);
    }

    #[test]
    fn argv_matches_the_wrapper() {
        let invocation = bare(Provider::Codex);
        let (argv, note) = codex_argv(&invocation, "gpt-6.1-sol", "medium", Some("hello"));
        assert!(note.is_none());
        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "-c",
                "approval_policy=\"never\"",
                "--dangerously-bypass-approvals-and-sandbox",
                "--json",
                "--skip-git-repo-check",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=\"medium\"",
                "--",
                "hello",
            ]
        );

        let (argv, note) = codex_argv(&invocation, "gpt-6.1-sol", "max", Some("hello"));
        assert_eq!(flag(&argv, "-c"), Some("approval_policy=\"never\""));
        assert!(
            argv.iter()
                .any(|arg| arg == "model_reasoning_effort=\"ultra\"")
        );
        assert_eq!(note.as_deref(), Some("[crew] effort max -> ultra"));

        let (argv, note) = codex_argv(&invocation, "gpt-6-luna", "max", Some("hello"));
        assert!(
            argv.iter()
                .any(|arg| arg == "model_reasoning_effort=\"max\"")
        );
        assert!(note.is_none());

        let mut invocation = bare(Provider::Codex);
        assert!(has(&argv, "--dangerously-bypass-approvals-and-sandbox"));

        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"));
        assert_eq!(argv.last().map(String::as_str), Some("hello"));

        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", None);
        assert!(!has(&argv, "--"));

        invocation.resume = Some("abc".to_string());
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"));
        assert!(has(&argv, "resume"));
        assert!(argv.iter().any(|arg| arg == "abc"));
        assert!(!has(&argv, "--last"));

        invocation.resume = None;
        invocation.continue_session = true;
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"));
        assert!(has(&argv, "resume"));
        assert!(has(&argv, "--last"));

        invocation.continue_session = false;
        invocation.review = true;
        invocation.uncommitted = true;
        assert!(codex_scope(&invocation));
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", None);
        assert!(has(&argv, "review"));
        assert!(has(&argv, "--uncommitted"));
        assert!(!has(&argv, "--"));

        invocation.uncommitted = false;
        invocation.schema = Some("{".to_string());
        invocation.add_dir = vec!["~/nope".into()];
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"));
        assert_eq!(argv.last().map(String::as_str), Some("hello"));
        let schema = flag(&argv, "--output-schema").unwrap();
        assert!(Path::new(schema).is_absolute());
        assert!(schema.contains('{'));
        let directory = flag(&argv, "--add-dir").unwrap();
        assert!(directory.contains('~'));
        assert!(Path::new(directory).is_absolute());
    }
}
