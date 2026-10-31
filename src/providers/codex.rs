//! Codex headless runner.
//!
//! User-facing `max` is `ultra` on the wire, except `gpt-6-luna`, which has no
//! `ultra`. Any `turn.failed` fails the run, including one followed by
//! `turn.completed`. The deadline SIGKILLs the process group immediately.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

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
    let prompt = take_prompt(invocation)?;
    // Review stays on `codex exec`: app-server steer does not cover it.
    // `-c` has no thread id yet, so it stays on exec too.
    if crate::job::inbox::job_name().is_some() && !invocation.review && !invocation.continue_session
    {
        return run_app_server(invocation, prompt.as_deref());
    }
    let (argv, note) = plan(invocation, prompt.as_deref())?;
    proc::write_effort_note(&note);
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let mut watch = |line: &str| proc::session_from_line(line);
    let captured = match proc::launch_or_note(
        "codex",
        proc::capture(
            &argv,
            invocation.cwd.as_deref(),
            timeout,
            StdioKind::Piped,
            KillStyle::ImmediateKill,
            &mut watch,
        ),
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    Ok(emit_codex(
        invocation,
        &captured,
        &parse_events(&captured.stdout),
    ))
}

fn scope_count(invocation: &Invocation) -> usize {
    usize::from(invocation.uncommitted)
        + usize::from(invocation.base.is_some())
        + usize::from(invocation.commit.is_some())
}

fn plan(
    invocation: &Invocation,
    prompt: Option<&str>,
) -> Result<(Vec<String>, Option<String>), Error> {
    let (model, effort, verbatim) = model_effort(invocation)?;
    Ok(codex_argv(
        invocation,
        &model,
        &effort,
        prompt,
        verbatim.as_deref(),
    ))
}

fn model_effort(invocation: &Invocation) -> Result<(String, String, Option<String>), Error> {
    let resolved = tier::resolve(
        invocation.provider,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let model = resolved
        .model
        .ok_or_else(|| Error::new(2, "codex needs a model"))?;
    let verbatim = proc::effort_verbatim(invocation, &resolved.effort)?;
    Ok((model, resolved.effort, verbatim))
}

fn emit_codex(invocation: &Invocation, captured: &proc::Captured, parsed: &CodexParse) -> i32 {
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
    proc::emit_report(
        &Report {
            code,
            before_stderr: before,
            stdout,
            after_stderr: after,
        },
        captured.session_id.as_deref(),
    );
    code
}

pub(crate) fn cli_model(invocation: &Invocation) -> Result<Option<String>, Error> {
    let (argv, _) = plan(invocation, None)?;
    Ok(proc::model_arg(&argv))
}

pub(super) fn codex_argv(
    invocation: &Invocation,
    model: &str,
    effort: &str,
    prompt: Option<&str>,
    verbatim: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let (wire, note) = map_effort(Provider::Codex, Some(model), effort, verbatim);
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
    finish_parse(message, thread_id, completed, failed, notices, usage_limit)
}

fn finish_parse(
    message: String,
    thread_id: Option<String>,
    completed: bool,
    failed: bool,
    notices: Vec<String>,
    usage_limit: Option<String>,
) -> CodexParse {
    CodexParse {
        success: completed && !failed && !message.trim().is_empty(),
        message,
        thread_id,
        notices,
        usage_limit,
    }
}

fn run_app_server(invocation: &Invocation, prompt: Option<&str>) -> Result<i32, Error> {
    let prompt = prompt.ok_or_else(|| {
        Error::new(
            2,
            "empty prompt (pass a prompt argument or pipe one via stdin)",
        )
    })?;
    let (model, effort, verbatim) = model_effort(invocation)?;
    let (wire, note) = map_effort(Provider::Codex, Some(&model), &effort, verbatim.as_deref());
    proc::write_effort_note(&note);
    let schema = schema_value(invocation)?;
    let cwd = invocation
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok());
    let job = crate::job::inbox::job_name().unwrap_or_default();
    let mut rpc = CodexRpc {
        dir: crate::job::store::run_dir(&job),
        prompt: prompt.to_string(),
        model,
        effort: wire,
        cwd: cwd.as_ref().map(|path| path.to_string_lossy().into_owned()),
        schema,
        resume: invocation.resume.clone(),
        thread_id: None,
        turn_id: None,
        sent_init: false,
        sent_turn: false,
        steers: Vec::new(),
        next_id: 1003,
    };
    let argv = vec!["codex".to_string(), "app-server".to_string()];
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let mut watch = |line: &str| app_session(line);
    let captured = match proc::launch_or_note(
        "codex",
        proc::capture_driven(
            &argv,
            invocation.cwd.as_deref(),
            timeout,
            KillStyle::ImmediateKill,
            &mut watch,
            |tick| codex_tick(&mut rpc, tick),
        ),
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    Ok(emit_codex(
        invocation,
        &captured,
        &parse_app(&captured.stdout),
    ))
}

struct CodexRpc {
    dir: std::path::PathBuf,
    prompt: String,
    model: String,
    effort: String,
    cwd: Option<String>,
    schema: Option<Value>,
    resume: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    sent_init: bool,
    sent_turn: bool,
    steers: Vec<(i64, u64)>,
    next_id: i64,
}

fn codex_tick(state: &mut CodexRpc, tick: &mut proc::Tick<'_>) -> proc::TickDone {
    if !state.sent_init {
        state.sent_init = true;
        tick.send(&rpc(
            "initialize",
            1000,
            json!({"clientInfo": {"name": "crew", "title": "crew", "version": "0.1.0"}}),
        ));
        tick.send(r#"{"method":"initialized","params":{}}"#);
    }
    let lines = tick.lines.clone();
    for line in &lines {
        if let Some(reply) = approval_reply(line) {
            tick.send(&reply);
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = event.get("id").and_then(Value::as_i64);
        if id == Some(1000) && event.get("result").is_some() {
            tick.send(&thread_request(state));
        }
        if id == Some(1001)
            && event.get("error").is_none()
            && let Some(thread) = event.pointer("/result/thread/id").and_then(Value::as_str)
        {
            state.thread_id = Some(thread.to_string());
            if !state.sent_turn {
                state.sent_turn = true;
                tick.send(&turn_request(state, thread));
            }
        }
        if let Some(turn) = event
            .pointer("/params/turn/id")
            .and_then(Value::as_str)
            .filter(|_| event.get("method").and_then(Value::as_str) == Some("turn/started"))
            .or_else(|| {
                (id == Some(1002))
                    .then(|| event.pointer("/result/turn/id").and_then(Value::as_str))
                    .flatten()
            })
        {
            state.turn_id = Some(turn.to_string());
        }
        if let Some(rpc_id) = id
            && event.get("error").is_none()
            && let Some(pos) = state.steers.iter().position(|(rid, _)| *rid == rpc_id)
        {
            let (_, n) = state.steers.remove(pos);
            let _ = crate::job::inbox::mark_delivered(&state.dir, n);
        }
        if event.get("method").and_then(Value::as_str) == Some("turn/completed") {
            return proc::TickDone::Done { stop: true };
        }
    }
    if let (Some(thread), Some(turn)) = (state.thread_id.clone(), state.turn_id.clone())
        && let Ok(pending) = crate::job::inbox::steers(&state.dir)
    {
        for item in pending {
            if state.steers.iter().any(|(_, n)| *n == item.n) {
                continue;
            }
            let rpc_id = state.next_id;
            state.next_id += 1;
            state.steers.push((rpc_id, item.n));
            tick.send(&rpc(
                "turn/steer",
                rpc_id,
                json!({
                    "threadId": thread,
                    "expectedTurnId": turn,
                    "input": [{"type": "text", "text": item.text}]
                }),
            ));
        }
    }
    proc::TickDone::Continue
}

fn rpc(method: &str, id: i64, params: Value) -> String {
    json!({"method": method, "id": id, "params": params}).to_string()
}

/// Same bypass as `codex exec`: never ask, and full access.
fn policy(cwd: &Option<String>) -> Value {
    let mut params = json!({
        "approvalPolicy": "never",
        "sandboxPolicy": {"type": "dangerFullAccess"},
    });
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd);
    }
    params
}

fn thread_request(state: &CodexRpc) -> String {
    let mut params = policy(&state.cwd);
    params["model"] = json!(state.model);
    if let Some(thread) = &state.resume {
        params["threadId"] = json!(thread);
        return rpc("thread/resume", 1001, params);
    }
    rpc("thread/start", 1001, params)
}

fn turn_request(state: &CodexRpc, thread: &str) -> String {
    let mut params = policy(&state.cwd);
    params["threadId"] = json!(thread);
    params["input"] = json!([{"type": "text", "text": state.prompt}]);
    params["model"] = json!(state.model);
    params["effort"] = json!(state.effort);
    if let Some(schema) = &state.schema {
        params["outputSchema"] = schema.clone();
    }
    rpc("turn/start", 1002, params)
}

fn approval_reply(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    let method = value.get("method").and_then(Value::as_str)?;
    if !(method.contains("requestApproval")
        || method.contains("requestUserInput")
        || method.contains("elicitation"))
    {
        return None;
    }
    let id = value.get("id")?;
    Some(json!({"id": id, "result": {"decision": "accept", "action": "accept"}}).to_string())
}

fn text_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn app_session(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if let Some(id) = text_at(&value, "/result/thread/id") {
        return Some(id);
    }
    if value.get("method").and_then(Value::as_str) == Some("thread/started") {
        return text_at(&value, "/params/thread/id");
    }
    None
}

fn schema_value(invocation: &Invocation) -> Result<Option<Value>, Error> {
    let Some(schema) = &invocation.schema else {
        return Ok(None);
    };
    let path = proc::resolve_no_tilde(Path::new(schema));
    let text =
        fs::read_to_string(&path).map_err(|err| Error::new(2, format!("--schema: {err}")))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| Error::new(2, format!("--schema: {err}")))
}

fn parse_app(stdout: &str) -> CodexParse {
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
        if let Some(id) = app_session(line) {
            thread_id = Some(id);
        }
        if let Some(limit) = app_limit(&event) {
            usage_limit = Some(limit);
        }
        match event.get("method").and_then(Value::as_str) {
            Some("turn/completed") => {
                match event
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                {
                    "completed" => completed = true,
                    "failed" => {
                        failed = true;
                        notices.push(line.to_string());
                    }
                    _ => {}
                }
            }
            Some("error") => notices.push(line.to_string()),
            Some("item/completed") => {
                let Some(item) = event.pointer("/params/item") else {
                    continue;
                };
                // Commentary is the model's aside, not the answer shown to the caller.
                if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && item.get("phase").and_then(Value::as_str) != Some("commentary")
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    message = text.to_string();
                }
            }
            _ => {}
        }
    }
    finish_parse(message, thread_id, completed, failed, notices, usage_limit)
}

fn app_limit(event: &Value) -> Option<String> {
    let blob = event.to_string();
    if !blob.contains("UsageLimit") && !looks_like_usage_limit(&blob) {
        return None;
    }
    [
        "/params/turn/error/message",
        "/params/error/message",
        "/error/message",
    ]
    .into_iter()
    .find_map(|pointer| {
        event
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
    })
    .or(Some(blob))
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
        let (argv, note) = codex_argv(&invocation, "gpt-6.1-sol", "medium", Some("hello"), None);
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

        let (argv, note) = codex_argv(&invocation, "gpt-6.1-sol", "max", Some("hello"), None);
        assert_eq!(flag(&argv, "-c"), Some("approval_policy=\"never\""));
        assert!(
            argv.iter()
                .any(|arg| arg == "model_reasoning_effort=\"ultra\"")
        );
        assert_eq!(note.as_deref(), Some("[crew] effort max -> ultra"));

        let (argv, note) = codex_argv(&invocation, "gpt-6-luna", "max", Some("hello"), None);
        assert!(
            argv.iter()
                .any(|arg| arg == "model_reasoning_effort=\"max\"")
        );
        assert!(note.is_none());

        let mut invocation = bare(Provider::Codex);
        assert!(has(&argv, "--dangerously-bypass-approvals-and-sandbox"));

        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"), None);
        assert_eq!(argv.last().map(String::as_str), Some("hello"));

        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", None, None);
        assert!(!has(&argv, "--"));

        invocation.resume = Some("abc".to_string());
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"), None);
        assert!(has(&argv, "resume"));
        assert!(argv.iter().any(|arg| arg == "abc"));
        assert!(!has(&argv, "--last"));

        invocation.resume = None;
        invocation.continue_session = true;
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"), None);
        assert!(has(&argv, "resume"));
        assert!(has(&argv, "--last"));

        invocation.continue_session = false;
        invocation.review = true;
        invocation.uncommitted = true;
        assert!(codex_scope(&invocation));
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", None, None);
        assert!(has(&argv, "review"));
        assert!(has(&argv, "--uncommitted"));
        assert!(!has(&argv, "--"));

        invocation.uncommitted = false;
        invocation.schema = Some("{".to_string());
        invocation.add_dir = vec!["~/nope".into()];
        let (argv, _) = codex_argv(&invocation, "gpt-6.1-sol", "low", Some("hello"), None);
        assert_eq!(argv.last().map(String::as_str), Some("hello"));
        let schema = flag(&argv, "--output-schema").unwrap();
        assert!(Path::new(schema).is_absolute());
        assert!(schema.contains('{'));
        let directory = flag(&argv, "--add-dir").unwrap();
        assert!(directory.contains('~'));
        assert!(Path::new(directory).is_absolute());
    }
}
