//! Claude Code headless runner.
//!
//! stream-json prints the session id on the init event, before the turn
//! finishes, so a killed run can still be resumed. A denied tool still makes
//! Claude exit 0. That becomes 125, because a caller checking the status
//! would otherwise treat the denial as a finished answer.

use serde_json::{Map, Value};

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{
    self, KillStyle, Report, StdioKind, USAGE_LIMIT_EXIT, code_or_one, looks_like_usage_limit,
    map_effort, py_display, py_truthy, shown, take_prompt, usage_limit_line,
};

pub fn run(invocation: &Invocation) -> Result<i32, Error> {
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
    let (argv, note) = plan(invocation, &prompt, schema.as_deref())?;
    proc::write_effort_note(&note);
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let mut watch = |line: &str| proc::session_from_line(line);
    let job = crate::job::inbox::job_name();
    let captured = match proc::launch_or_note(
        "claude",
        if let Some(job) = job {
            let dir = crate::job::store::run_dir(&job);
            let mut steer = ClaudeSteer {
                dir,
                prompt: prompt.clone(),
                opened: false,
                skip_abort: false,
                seq: 0,
            };
            proc::capture_driven(
                &job_argv(argv),
                invocation.cwd.as_deref(),
                timeout,
                KillStyle::TermThenKill,
                &mut watch,
                |tick| claude_tick(&mut steer, tick),
            )
        } else {
            proc::capture(
                &argv,
                invocation.cwd.as_deref(),
                timeout,
                StdioKind::Piped,
                KillStyle::TermThenKill,
                &mut watch,
            )
        },
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    let schema_set = schema.as_ref().is_some_and(|text| !text.is_empty());
    let report = assess_claude(
        invocation.raw,
        schema_set,
        captured.code,
        &captured.stdout,
        &captured.stderr,
    );
    let report = proc::keep_timeout(report, captured.timed_out, invocation.timeout_secs);
    proc::emit_report(&report, captured.session_id.as_deref());
    Ok(report.code)
}

/// Same argv `run` launches, so the recorded model cannot drift from the CLI.
fn plan(
    invocation: &Invocation,
    prompt: &str,
    schema: Option<&str>,
) -> Result<(Vec<String>, Option<String>), Error> {
    let resolved = tier::resolve(
        invocation.provider,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let model = resolved
        .model
        .ok_or_else(|| Error::new(2, "claude needs a model"))?;
    let verbatim = proc::effort_verbatim(invocation, &resolved.effort)?;
    Ok(claude_argv(
        invocation,
        &model,
        &resolved.effort,
        prompt,
        schema,
        verbatim.as_deref(),
    ))
}

pub(crate) fn cli_model(invocation: &Invocation) -> Result<Option<String>, Error> {
    let (argv, _) = plan(invocation, "", None)?;
    Ok(proc::model_arg(&argv))
}

pub(super) fn claude_argv(
    invocation: &Invocation,
    model: &str,
    effort: &str,
    prompt: &str,
    schema: Option<&str>,
    verbatim: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let (wire, note) = map_effort(Provider::Claude, Some(model), effort, verbatim);
    // stream-json refuses to start without --verbose, so the init event
    // (and its session id) would never be printed.
    let mut argv = vec![
        "claude".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        "--permission-prompts".to_string(),
        "none".to_string(),
    ];
    argv.push("--dangerously-skip-permissions".to_string());
    if invocation.no_web {
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

/// Job runs speak stream-json on stdin so a steer can land mid-turn.
/// The prompt is a user message, not an argv the CLI would treat as one shot.
fn job_argv(mut argv: Vec<String>) -> Vec<String> {
    if argv.len() >= 2 && argv[argv.len() - 2] == "-p" {
        argv.pop();
        argv.pop();
    }
    argv.push("-p".to_string());
    argv.push("--input-format".to_string());
    argv.push("stream-json".to_string());
    argv
}

struct ClaudeSteer {
    dir: std::path::PathBuf,
    prompt: String,
    opened: bool,
    /// An interrupt ends the in-flight tool with its own result event.
    /// That event is not the answer; the user message that follows is.
    skip_abort: bool,
    seq: u64,
}

fn claude_tick(state: &mut ClaudeSteer, tick: &mut proc::Tick<'_>) -> proc::TickDone {
    if !state.opened {
        state.opened = true;
        tick.send(&claude_user(&state.prompt));
    }
    for line in &tick.lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        if state.skip_abort && claude_abort(&value) {
            state.skip_abort = false;
            continue;
        }
        // The turn is over. A steer that shows up now stays in the inbox
        // and the supervisor resumes it, instead of starting another turn here.
        return proc::TickDone::Done { stop: false };
    }
    if let Ok(pending) = crate::job::inbox::steers(&state.dir) {
        for item in pending {
            state.seq = state.seq.saturating_add(1);
            tick.send(&claude_interrupt(state.seq));
            tick.send(&claude_user(&item.text));
            let _ = crate::job::inbox::mark_delivered(&state.dir, item.n);
            state.skip_abort = true;
        }
    }
    proc::TickDone::Continue
}

fn claude_user(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    })
    .to_string()
}

fn claude_interrupt(n: u64) -> String {
    serde_json::json!({
        "type": "control_request",
        "request_id": format!("crew-{n}"),
        "request": {"subtype": "interrupt"}
    })
    .to_string()
}

fn claude_abort(value: &Value) -> bool {
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    subtype == "error_during_execution" || subtype.contains("abort")
}

pub(super) fn assess_claude(raw: bool, schema: bool, child: i32, out: &str, err: &str) -> Report {
    if let Some(events) = claude_events(out) {
        return assess_claude_events(raw, schema, child, out, err, &events);
    }
    assess_claude_object(raw, schema, child, out, err)
}

/// Lines the model has already streamed, when the buffer is not the final
/// single JSON object. The init line's session id is enough to resume.
fn claude_events(out: &str) -> Option<Vec<Value>> {
    if let Ok(value) = serde_json::from_str::<Value>(out.trim())
        && value.is_object()
        && !is_live_claude_event(&value)
    {
        return None;
    }
    let mut events = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(out.trim())
        && value.is_object()
    {
        events.push(value);
        return Some(events);
    }
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && value.is_object()
        {
            events.push(value);
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn is_live_claude_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "system"
                | "assistant"
                | "user"
                | "stream_event"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_start"
                | "message_delta"
                | "message_stop"
        )
    )
}

fn assess_claude_events(
    raw: bool,
    schema: bool,
    child: i32,
    out: &str,
    err: &str,
    events: &[Value],
) -> Report {
    let mut session_id = None;
    let mut result = None;
    let mut deltas = String::new();
    let mut assistant = String::new();
    let mut saw_delta = false;
    for event in events {
        if session_id.is_none() {
            session_id = proc::session_from_line(&event.to_string());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("result") => result = Some(event),
            Some("assistant") if !saw_delta => append_content_text(event, &mut assistant),
            Some("stream_event" | "content_block_delta" | "message_delta")
                if append_delta_text(event, &mut deltas) =>
            {
                saw_delta = true;
            }
            _ => {}
        }
    }
    if let Some(result) = result {
        let mut owned = result.clone();
        if owned
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            && let Some(id) = &session_id
            && let Some(object) = owned.as_object_mut()
        {
            object.insert("session_id".to_string(), Value::String(id.clone()));
        }
        let rendered = serde_json::to_string(&owned).unwrap_or_default();
        let mut report = assess_claude_object(raw, schema, child, &rendered, err);
        if raw {
            report.stdout = out.to_string();
        }
        return report;
    }
    let text = if saw_delta { deltas } else { assistant };
    let mut before = String::new();
    if !err.trim().is_empty() {
        before.push_str(err);
        if !err.ends_with('\n') {
            before.push('\n');
        }
    }
    let mut stdout = String::new();
    if raw {
        stdout.push_str(out);
    } else if !text.trim().is_empty() {
        stdout.push_str(text.trim_end());
        stdout.push('\n');
    }
    let mut after = String::new();
    if let Some(id) = session_id.filter(|id| !id.is_empty()) {
        after.push_str(&format!("SESSION_ID={id}\n"));
    }
    report(child, before, stdout, after)
}

fn append_content_text(event: &Value, buf: &mut String) {
    let Some(blocks) = event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str);
        if matches!(kind, Some("text" | "thinking"))
            && let Some(text) = block
                .get("text")
                .or_else(|| block.get("thinking"))
                .and_then(Value::as_str)
        {
            buf.push_str(text);
        }
    }
}

fn append_delta_text(event: &Value, buf: &mut String) -> bool {
    let delta = event
        .get("delta")
        .or_else(|| event.get("event").and_then(|inner| inner.get("delta")));
    let Some(delta) = delta else {
        return false;
    };
    let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(kind, "signature_delta" | "input_json_delta") {
        return false;
    }
    // Thinking is the only text a run may have produced when the deadline
    // hits before the answer. A later result event still replaces it.
    let text = delta
        .get("text")
        .or_else(|| delta.get("thinking"))
        .and_then(Value::as_str);
    let Some(text) = text else {
        return false;
    };
    buf.push_str(text);
    true
}

fn assess_claude_object(raw: bool, schema: bool, child: i32, out: &str, err: &str) -> Report {
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
                if looks_like_usage_limit(out) && child != 0 {
                    return limit_report(out, before, stdout);
                }
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
    if let Some(id) = data.get("session_id").and_then(Value::as_str)
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
        after.push_str(&format!("[crew] tool {names} auto-denied.\n"));
        return report(125, before, stdout, after);
    }
    if claude_failed(&data, &text) {
        if hit_usage_limit(&data, &text) {
            after.push_str(&usage_limit_line("claude", &text));
            return report(USAGE_LIMIT_EXIT, before, stdout, after);
        }
        let detail = failure_detail(&data, &text);
        let suffix = errors_suffix(&data);
        after.push_str(&format!("[crew] run did not complete: {detail}.{suffix}\n"));
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

/// Claude exits 0 for an error result, so the reason has to come from the
/// JSON. A `success` subtype is never the reason: it is what the CLI reports
/// even when `is_error` is set.
fn failure_detail(data: &Value, text: &str) -> String {
    if let Some(subtype) = data
        .get("subtype")
        .and_then(Value::as_str)
        .filter(|subtype| *subtype != "success")
    {
        return subtype.to_string();
    }
    if py_truthy(data.get("is_error")) {
        return "claude reported an error".to_string();
    }
    if text.trim().is_empty() {
        return "empty answer".to_string();
    }
    if py_truthy(data.get("terminal_reason")) {
        return py_display(data.get("terminal_reason"));
    }
    "incomplete".to_string()
}

/// A failed result is a usage limit when the API said 429 or the message is
/// the CLI's own limit notice (synthetic, so it carries no status).
fn hit_usage_limit(data: &Value, text: &str) -> bool {
    data.get("api_error_status").and_then(Value::as_u64) == Some(429)
        || looks_like_usage_limit(text)
}

fn limit_report(message: &str, before: String, stdout: String) -> Report {
    report(
        USAGE_LIMIT_EXIT,
        before,
        stdout,
        usage_limit_line("claude", message),
    )
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
    let input = shown(usage.and_then(|value| value.get("input_tokens")));
    let output = shown(usage.and_then(|value| value.get("output_tokens")));
    let turns = shown(data.get("num_turns"));
    let cost = shown(data.get("total_cost_usd"));
    if input.is_none() && output.is_none() && turns.is_none() && cost.is_none() {
        return None;
    }
    let mut line = String::from("[crew]");
    if let Some(input) = input {
        line.push_str(&format!(" in={input}"));
    }
    if let Some(output) = output {
        line.push_str(&format!(" out={output}"));
    }
    if let Some(turns) = turns {
        line.push_str(&format!(" turns={turns}"));
    }
    if let Some(cost) = cost {
        line.push_str(&format!(" cost=${cost}"));
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
        let (argv, note) = claude_argv(&invocation, "sonnet", "medium", "hello", None, None);
        assert!(note.is_none());
        assert_eq!(
            argv,
            vec![
                "claude",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--verbose",
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

        let (argv, note) = claude_argv(&invocation, "opus", "max", "hello", None, None);
        assert!(note.is_none());
        assert_eq!(flag(&argv, "--effort"), Some("max"));

        invocation.no_web = true;
        invocation.add_dir = vec!["~/extra".into()];
        let (argv, _) = claude_argv(&invocation, "sonnet", "medium", "hello", None, None);
        assert_eq!(
            flag(&argv, "--disallowed-tools"),
            Some("WebSearch,WebFetch")
        );
        assert!(has(&argv, "--dangerously-skip-permissions"));
        assert_eq!(flag(&argv, "--add-dir"), Some("~/extra"));
    }

    #[test]
    fn denial_and_empty_result_are_125() {
        let denied = r#"{"type":"result","subtype":"success","is_error":false,"result":"answer","session_id":"abc","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Bash"},{"tool_name":"Write"}]}"#;
        let report = assess_claude(false, false, 0, denied, "");
        assert_eq!(report.code, 125);
        assert_eq!(report.stdout, "answer\n");
        assert!(report.after_stderr.contains("SESSION_ID=abc"));
        assert!(report.after_stderr.contains("'Bash', 'Write'"));
        assert!(!report.after_stderr.contains("'Bash', 'Bash'"));

        let empty = r#"{"type":"result","subtype":"success","is_error":false,"result":"  ","permission_denials":[]}"#;
        let report = assess_claude(false, false, 0, empty, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("empty answer"));
        assert!(!report.after_stderr.contains("success"));

        let errored = r#"{"type":"result","subtype":"success","is_error":true,"result":"nope","permission_denials":[]}"#;
        assert_eq!(assess_claude(false, false, 0, errored, "").code, 125);
    }

    #[test]
    fn usage_limit_is_its_own_exit_and_never_success() {
        let limit = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"You've hit your session limit · resets 7:40pm (Europe/Bratislava)","permission_denials":[]}"#;
        let report = assess_claude(false, false, 0, limit, "");
        assert_eq!(report.code, USAGE_LIMIT_EXIT);
        assert!(report.after_stderr.contains(
            "[crew] claude usage limit: You've hit your session limit · resets 7:40pm (Europe/Bratislava)"
        ));
        assert!(!report.after_stderr.contains("success"));

        let credits = r#"{"type":"result","subtype":"success","is_error":true,"result":"You're out of usage credits. Switch to another model.","num_turns":1,"permission_denials":[]}"#;
        assert_eq!(
            assess_claude(false, false, 0, credits, "").code,
            USAGE_LIMIT_EXIT
        );
        let plain = "You've hit your session limit · resets 7:40pm";
        assert_eq!(
            assess_claude(false, false, 1, plain, "").code,
            USAGE_LIMIT_EXIT
        );
    }

    #[test]
    fn stream_keeps_the_session_and_partial_text() {
        let out = "\
{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-1\"}\n\
{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"one\\n\"}},\"session_id\":\"sess-1\"}\n";
        assert_eq!(
            proc::session_from_line(out.lines().next().unwrap()).as_deref(),
            Some("sess-1")
        );
        let report = assess_claude(false, false, 143, out, "");
        assert!(report.after_stderr.contains("SESSION_ID=sess-1"));
        assert!(report.stdout.contains("one"));
        let report = proc::keep_timeout(report, true, 20.0);
        assert_eq!(report.code, 124);
        assert!(report.after_stderr.contains("timed out after 20s"));
        assert!(report.stdout.contains("one"));
    }

    #[test]
    fn success_keeps_the_child_status() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","permission_denials":[]}"#;
        assert_eq!(assess_claude(false, false, 0, ok, "").code, 0);
        let report = assess_claude(false, false, 1, ok, "");
        assert_eq!(report.code, 1);
        assert_eq!(report.stdout, "ok\n");
    }
}
