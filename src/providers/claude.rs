//! Claude Code headless runner.
//!
//! A denied tool still makes Claude exit 0. That becomes 125, because a caller
//! checking the status would otherwise treat the denial as a finished answer.

use std::time::Instant;

use serde_json::{Map, Value};

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{
    self, KillStyle, Report, StdioKind, code_or_one, format_secs, map_effort, py_display,
    py_truthy, take_prompt,
};

const READ_RULE: &str = "This is a read-only session. You may use Read, Glob, Grep, and web tools only. File edits, shell commands, and subagents are unavailable. Do not hunt for workarounds. If the task needs a modification or a shell command, say so and stop.";

pub fn run(invocation: &Invocation) -> Result<i32, Error> {
    let resolved = tier::resolve(
        invocation.provider,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let model = resolved
        .model
        .ok_or_else(|| Error::new(2, "claude needs a model"))?;
    let prompt = take_prompt(invocation)?.ok_or_else(|| {
        Error::new(
            2,
            "empty prompt (pass a prompt argument or pipe one via stdin)",
        )
    })?;
    let schema = match &invocation.schema {
        Some(schema) => Some(proc::load_schema_literal(schema)?),
        None => None,
    };
    let (argv, note) = claude_argv(
        invocation,
        &model,
        &resolved.effort,
        &prompt,
        schema.as_deref(),
    );
    proc::write_effort_note(&note);
    let started = Instant::now();
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let captured = match proc::launch_or_note(
        proc::capture(
            &argv,
            invocation.cwd.as_deref(),
            timeout,
            StdioKind::Piped,
            KillStyle::TermThenKill,
            true,
        ),
        started,
        &model,
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    if captured.timed_out {
        proc::write_stderr_line(&format!(
            "[crew] timed out after {}s",
            format_secs(invocation.timeout_secs)
        ));
        proc::note_long_run(started, &model);
        return Ok(124);
    }
    let schema_set = schema.as_ref().is_some_and(|text| !text.is_empty());
    let report = assess_claude(
        invocation.raw,
        invocation.print_id,
        schema_set,
        captured.code,
        &captured.stdout,
        &captured.stderr,
    );
    proc::emit_report(&report);
    proc::note_long_run(started, &model);
    Ok(report.code)
}

pub(super) fn claude_argv(
    invocation: &Invocation,
    model: &str,
    effort: &str,
    prompt: &str,
    schema: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let (wire, note) = map_effort(Provider::Claude, Some(model), effort);
    let mut argv = vec![
        "claude".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--permission-prompts".to_string(),
        "none".to_string(),
    ];
    if invocation.write {
        proc::push_pair(&mut argv, "--permission-mode", "auto");
    } else if invocation.read_only {
        let tools = claude_tools(invocation.no_web);
        proc::push_pair(&mut argv, "--permission-mode", "dontAsk");
        proc::push_pair(&mut argv, "--tools", &tools);
        proc::push_pair(&mut argv, "--allowed-tools", &tools);
        argv.push("--strict-mcp-config".to_string());
        proc::push_pair(&mut argv, "--append-system-prompt", READ_RULE);
    } else {
        argv.push("--dangerously-skip-permissions".to_string());
    }
    if invocation.no_web && !invocation.read_only {
        proc::push_pair(&mut argv, "--disallowed-tools", "WebSearch,WebFetch");
    }
    proc::push_pair(&mut argv, "--model", model);
    proc::push_pair(&mut argv, "--effort", &wire);
    for directory in &invocation.add_dir {
        let directory = directory.to_string_lossy();
        proc::push_pair(&mut argv, "--add-dir", &directory);
    }
    if let Some(schema) = schema {
        proc::push_pair(&mut argv, "--json-schema", schema);
    }
    if let Some(resume) = &invocation.resume {
        proc::push_pair(&mut argv, "--resume", resume);
    } else if invocation.continue_session {
        argv.push("--continue".to_string());
    }
    proc::push_pair(&mut argv, "-p", prompt);
    (argv, note)
}

fn claude_tools(no_web: bool) -> String {
    ["Read", "Glob", "Grep", "WebSearch", "WebFetch"]
        .into_iter()
        .filter(|name| !(no_web && name.starts_with("Web")))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn assess_claude(
    raw: bool,
    print_id: bool,
    schema: bool,
    child: i32,
    out: &str,
    err: &str,
) -> Report {
    let mut before = String::new();
    let mut stdout = String::new();
    if !err.trim().is_empty() {
        before.push_str(err);
        if !err.ends_with('\n') {
            before.push('\n');
        }
    }
    if raw {
        stdout.push_str(out);
    }
    let data = if out.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        match serde_json::from_str::<Value>(out) {
            Ok(value) => value,
            Err(_) => {
                if !raw {
                    push_trimmed_line(&mut stdout, out);
                }
                return report(code_or_one(child), before, stdout, String::new());
            }
        }
    };
    if !data.is_object() {
        if !raw {
            stdout.push_str(out);
        }
        return report(code_or_one(child), before, stdout, String::new());
    }
    let text = claude_answer(&data, schema);
    if !text.is_empty() && !raw {
        stdout.push_str(text.trim_end());
        stdout.push('\n');
    }
    let mut after = String::new();
    if print_id
        && let Some(id) = data.get("session_id").and_then(Value::as_str)
        && !id.is_empty()
    {
        after.push_str(&format!("SESSION_ID={id}\n"));
    }
    if let Some(line) = claude_usage_line(&data) {
        after.push_str(&line);
        after.push('\n');
    }
    let denials = denial_names(&data);
    if !denials.is_empty() {
        let names = denials
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", ");
        after.push_str(&format!(
            "[crew] tool {names} auto-denied. --read-only has no shell and cannot write. Drop the mode flag for full autonomy.\n"
        ));
        return report(125, before, stdout, after);
    }
    if claude_failed(&data, &text) {
        let detail = failure_detail(&data);
        let suffix = errors_suffix(&data);
        after.push_str(&format!("[crew] run ended early: {detail}.{suffix}\n"));
        return report(125, before, stdout, after);
    }
    report(child, before, stdout, after)
}

fn push_trimmed_line(stdout: &mut String, out: &str) {
    let trimmed = out.trim();
    if !trimmed.is_empty() {
        stdout.push_str(trimmed);
        stdout.push('\n');
    }
}

fn claude_answer(data: &Value, schema: bool) -> String {
    if schema
        && let Some(value) = data
            .get("structured_output")
            .filter(|value| !value.is_null())
    {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        return serde_json::to_string_pretty(value).unwrap_or_default();
    }
    match data.get("result") {
        Some(Value::String(text)) => text.trim().to_string(),
        _ => String::new(),
    }
}

fn claude_failed(data: &Value, text: &str) -> bool {
    data.get("type").and_then(Value::as_str) != Some("result")
        || data.get("subtype").and_then(Value::as_str) != Some("success")
        || py_truthy(data.get("is_error"))
        || text.trim().is_empty()
}

fn failure_detail(data: &Value) -> String {
    for key in ["subtype", "terminal_reason"] {
        if py_truthy(data.get(key)) {
            return py_display(data.get(key));
        }
    }
    "incomplete".to_string()
}

fn errors_suffix(data: &Value) -> String {
    let Some(items) = data.get("errors").and_then(Value::as_array) else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }
    let joined = items
        .iter()
        .map(|item| py_display(Some(item)))
        .collect::<Vec<_>>()
        .join("; ");
    format!(" {joined}")
}

fn denial_names(data: &Value) -> Vec<String> {
    let Some(items) = data.get("permission_denials").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for item in items {
        let name = if let Some(object) = item.as_object() {
            if py_truthy(object.get("tool_name")) {
                py_display(object.get("tool_name"))
            } else {
                item.to_string()
            }
        } else {
            py_display(Some(item))
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn claude_usage_line(data: &Value) -> Option<String> {
    let usage = data.get("usage").filter(|value| py_truthy(Some(value)));
    let cost = data.get("total_cost_usd").filter(|value| !value.is_null());
    if usage.is_none() && cost.is_none() {
        return None;
    }
    let mut line = format!(
        "[crew] in={} out={} turns={}",
        py_display(usage.and_then(|value| value.get("input_tokens"))),
        py_display(usage.and_then(|value| value.get("output_tokens"))),
        py_display(data.get("num_turns")),
    );
    if let Some(cost) = cost {
        line.push_str(&format!(" cost=${}", py_display(Some(cost))));
    }
    Some(line)
}

fn report(code: i32, before: String, stdout: String, after: String) -> Report {
    Report {
        code,
        before_stderr: before,
        stdout,
        after_stderr: after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::proc::{bare, flag, has};

    #[test]
    fn argv_matches_the_wrapper() {
        let mut invocation = bare(Provider::Claude);
        let (argv, note) = claude_argv(&invocation, "sonnet", "medium", "hello", None);
        assert!(note.is_none());
        assert_eq!(
            argv,
            vec![
                "claude",
                "--output-format",
                "json",
                "--permission-prompts",
                "none",
                "--dangerously-skip-permissions",
                "--model",
                "sonnet",
                "--effort",
                "medium",
                "-p",
                "hello",
            ]
        );

        let (argv, note) = claude_argv(&invocation, "opus", "max", "hello", None);
        assert!(note.is_none());
        assert_eq!(flag(&argv, "--effort"), Some("max"));

        invocation.read_only = true;
        invocation.no_web = true;
        invocation.add_dir = vec!["~/extra".into()];
        let (argv, _) = claude_argv(&invocation, "haiku", "high", "hello", None);
        assert_eq!(flag(&argv, "--permission-mode"), Some("dontAsk"));
        assert_eq!(flag(&argv, "--tools"), Some("Read,Glob,Grep"));
        assert_eq!(flag(&argv, "--allowed-tools"), Some("Read,Glob,Grep"));
        assert!(has(&argv, "--strict-mcp-config"));
        assert!(!has(&argv, "--disallowed-tools"));
        assert!(!has(&argv, "--dangerously-skip-permissions"));
        assert_eq!(flag(&argv, "--add-dir"), Some("~/extra"));

        invocation.read_only = false;
        invocation.write = false;
        let (argv, _) = claude_argv(&invocation, "sonnet", "medium", "hello", None);
        assert_eq!(
            flag(&argv, "--disallowed-tools"),
            Some("WebSearch,WebFetch")
        );
        assert!(has(&argv, "--dangerously-skip-permissions"));

        invocation.write = true;
        invocation.no_web = false;
        let (argv, _) = claude_argv(&invocation, "sonnet", "medium", "hello", None);
        assert_eq!(flag(&argv, "--permission-mode"), Some("auto"));
        assert!(!has(&argv, "--dangerously-skip-permissions"));
    }

    #[test]
    fn denial_and_empty_result_are_125() {
        let denied = r#"{"type":"result","subtype":"success","is_error":false,"result":"answer","session_id":"abc","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Bash"},{"tool_name":"Write"}]}"#;
        let report = assess_claude(false, true, false, 0, denied, "");
        assert_eq!(report.code, 125);
        assert_eq!(report.stdout, "answer\n");
        assert!(report.after_stderr.contains("SESSION_ID=abc"));
        assert!(report.after_stderr.contains("'Bash', 'Write'"));
        assert!(!report.after_stderr.contains("'Bash', 'Bash'"));

        let empty = r#"{"type":"result","subtype":"success","is_error":false,"result":"  ","permission_denials":[]}"#;
        let report = assess_claude(false, false, false, 0, empty, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("run ended early"));

        let errored = r#"{"type":"result","subtype":"success","is_error":true,"result":"nope","permission_denials":[]}"#;
        assert_eq!(assess_claude(false, false, false, 0, errored, "").code, 125);
    }

    #[test]
    fn success_keeps_the_child_status() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","permission_denials":[]}"#;
        assert_eq!(assess_claude(false, false, false, 0, ok, "").code, 0);
        let report = assess_claude(false, false, false, 1, ok, "");
        assert_eq!(report.code, 1);
        assert_eq!(report.stdout, "ok\n");
    }
}
