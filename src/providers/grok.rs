//! Grok headless runner.
//!
//! A missing `stopReason` is a finished turn. The read-only sandbox on this Mac
//! often refuses to start, so that one failure is retried with a tool allowlist
//! that still cannot write or spawn a shell.

use std::time::Instant;

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{
    self, KillStyle, Report, StdioKind, code_or_one, format_secs, map_effort, py_display,
    py_truthy, take_prompt,
};

const KERNEL_RULE: &str = "This is a read-only session. You can read files and run read-only shell commands, but any write fails at the OS level. Do not attempt writes and do not hunt for workarounds. If the task needs a modification, say so and stop.";
const FALLBACK_RULE: &str = "This is a read-only session with no shell. You may use read_file, grep, list_dir, and web tools only. File edits, shell commands, and subagents are unavailable. Do not hunt for workarounds. If the task needs a modification or a shell command, say so and stop.";

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
    let schema = match &invocation.schema {
        Some(schema) => Some(proc::load_schema_literal(schema)?),
        None => None,
    };
    let label = resolved
        .model
        .clone()
        .unwrap_or_else(|| "grok-4.7".to_string());
    let (argv, note) = grok_argv(
        invocation,
        resolved.model.as_deref(),
        &resolved.effort,
        &prompt,
        schema.as_deref(),
        true,
    );
    proc::write_effort_note(&note);
    let started = Instant::now();
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let launch = |argv: &[String]| {
        proc::launch_or_note(
            proc::capture(
                argv,
                None,
                timeout,
                StdioKind::Piped,
                KillStyle::TermThenKill,
                true,
            ),
            started,
            &label,
        )
    };
    let first = match launch(&argv) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    let captured = if should_retry_readonly(
        invocation.read_only,
        first.timed_out,
        first.code,
        &first.stderr,
    ) {
        let detail = sandbox_detail(&first.stderr);
        proc::write_stderr_line(&format!(
            "[crew] read-only sandbox did not start ({detail}). Using a read-only tool allowlist with no shell."
        ));
        let (argv, _) = grok_argv(
            invocation,
            resolved.model.as_deref(),
            &resolved.effort,
            &prompt,
            schema.as_deref(),
            false,
        );
        match launch(&argv) {
            Ok(captured) => captured,
            Err(code) => return Ok(code),
        }
    } else {
        first
    };
    if captured.timed_out {
        proc::write_stderr_line(&format!(
            "[crew] timed out after {}s",
            format_secs(invocation.timeout_secs)
        ));
        proc::note_long_run(started, &label);
        return Ok(124);
    }
    let schema_set = schema.as_ref().is_some_and(|text| !text.is_empty());
    let report = assess_grok(
        invocation.raw,
        invocation.print_id,
        schema_set,
        invocation.read_only,
        captured.code,
        &captured.stdout,
        &captured.stderr,
    );
    proc::emit_report(&report);
    proc::note_long_run(started, &label);
    Ok(report.code)
}

pub(super) fn grok_argv(
    invocation: &Invocation,
    model: Option<&str>,
    effort: &str,
    prompt: &str,
    schema: Option<&str>,
    readonly_sandbox: bool,
) -> (Vec<String>, Option<String>) {
    let (wire, note) = map_effort(Provider::Grok, model, effort);
    let mut argv = vec![
        "grok".to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "--no-auto-update".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--always-approve".to_string(),
    ];
    if invocation.write {
        proc::push_pair(&mut argv, "--sandbox", "workspace");
    } else if invocation.read_only && readonly_sandbox {
        proc::push_pair(&mut argv, "--sandbox", "read-only");
        proc::push_pair(&mut argv, "--rules", KERNEL_RULE);
    } else if invocation.read_only {
        proc::push_pair(&mut argv, "--tools", &fallback_tools(invocation.no_web));
        argv.push("--no-subagents".to_string());
        proc::push_pair(&mut argv, "--deny", "MCPTool");
        proc::push_pair(&mut argv, "--rules", FALLBACK_RULE);
    }
    if let Some(model) = model {
        proc::push_pair(&mut argv, "-m", model);
    }
    proc::push_pair(&mut argv, "--effort", &wire);
    if let Some(cwd) = &invocation.cwd {
        let cwd = cwd.to_string_lossy();
        proc::push_pair(&mut argv, "--cwd", &cwd);
    }
    if let Some(turns) = invocation.max_turns.filter(|turns| *turns > 0) {
        proc::push_pair(&mut argv, "--max-turns", &turns.to_string());
    }
    if let Some(schema) = schema {
        proc::push_pair(&mut argv, "--json-schema", schema);
    }
    if invocation.no_web {
        argv.push("--disable-web-search".to_string());
    }
    if let Some(resume) = &invocation.resume {
        proc::push_pair(&mut argv, "--resume", resume);
    } else if invocation.continue_session {
        argv.push("--continue".to_string());
    }
    (argv, note)
}

fn fallback_tools(no_web: bool) -> String {
    ["read_file", "grep", "list_dir", "web_search", "web_fetch"]
        .into_iter()
        .filter(|name| !(no_web && name.starts_with("web_")))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn should_retry_readonly(
    read_only: bool,
    timed_out: bool,
    code: i32,
    stderr: &str,
) -> bool {
    read_only && !timed_out && code != 0 && code != 124 && sandbox_refused(stderr)
}

fn sandbox_refused(stderr: &str) -> bool {
    stderr.contains("sandbox could not be applied")
        || (stderr.contains("could not apply the") && stderr.contains("sandbox profile"))
}

pub(super) fn sandbox_detail(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| line.to_ascii_lowercase().contains("sandbox"))
        .unwrap_or("read-only sandbox refused to start")
        .to_string()
}

pub(super) fn assess_grok(
    raw: bool,
    print_id: bool,
    schema: bool,
    read_only: bool,
    child: i32,
    out: &str,
    err: &str,
) -> Report {
    if raw {
        let mut after = String::new();
        if !err.trim().is_empty() {
            after.push_str(err);
        }
        return report(child, String::new(), out.to_string(), after);
    }
    let data = if out.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str::<Value>(out) {
            Ok(value) => value,
            Err(_) => {
                let mut stdout = String::new();
                let trimmed = out.trim();
                if !trimmed.is_empty() {
                    stdout.push_str(trimmed);
                    stdout.push('\n');
                }
                let mut after = String::new();
                if !err.trim().is_empty() {
                    after.push_str(err);
                }
                return report(code_or_one(child), String::new(), stdout, after);
            }
        }
    };
    if !data.is_object() {
        return report(
            code_or_one(child),
            String::new(),
            out.to_string(),
            String::new(),
        );
    }
    if data.get("type").and_then(Value::as_str) == Some("error") {
        let message = match data.get("message") {
            None => out.to_string(),
            Some(value) => py_display(Some(value)),
        };
        return report(
            code_or_one(child),
            String::new(),
            String::new(),
            format!("[crew] error: {message}\n"),
        );
    }
    let text = grok_answer(&data, schema);
    let mut stdout = String::new();
    if !text.is_empty() {
        stdout.push_str(text.trim_end());
        stdout.push('\n');
    }
    let mut after = String::new();
    if print_id
        && let Some(id) = data.get("sessionId").and_then(Value::as_str)
        && !id.is_empty()
    {
        after.push_str(&format!("SESSION_ID={id}\n"));
    }
    if let Some(line) = grok_usage_line(&data) {
        after.push_str(&line);
        after.push('\n');
    }
    if let Some(stop) = stop_reason(&data)
        && norm_stop(&stop) != "endturn"
    {
        let hint = early_stop_hint(&stop, read_only);
        after.push_str(&format!(
            "[crew] run ended early: stopReason={stop}.{hint}\n"
        ));
        return report(125, String::new(), stdout, after);
    }
    if child != 0 && !err.trim().is_empty() {
        after.push_str(err);
    }
    report(child, String::new(), stdout, after)
}

fn stop_reason(data: &Value) -> Option<String> {
    let value = data.get("stopReason")?;
    if !py_truthy(Some(value)) {
        return None;
    }
    Some(py_display(Some(value)))
}

pub(super) fn norm_stop(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn early_stop_hint(stop: &str, read_only: bool) -> &'static str {
    let key = norm_stop(stop);
    if key == "cancelled" && read_only {
        return " A tool call was blocked. --read-only cannot write or, when the read-only sandbox did not start, run a shell. Drop the mode flag for full autonomy.";
    }
    if key == "cancelled" {
        return " A tool call was blocked.";
    }
    if key.starts_with("maxturn") {
        return " Hit the turn cap; raise --max-turns or narrow the task.";
    }
    ""
}

fn grok_answer(data: &Value, schema: bool) -> String {
    if schema
        && let Some(value) = data
            .get("structuredOutput")
            .filter(|value| !value.is_null())
    {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        return serde_json::to_string_pretty(value).unwrap_or_default();
    }
    let text = data
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if schema { last_json_value(&text) } else { text }
}

fn last_json_value(text: &str) -> String {
    let mut index = 0;
    let mut last = None;
    while index < text.len() {
        let Some(relative) = text[index..].find('{') else {
            break;
        };
        let start = index + relative;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) => {
                let consumed = stream.byte_offset();
                if consumed == 0 {
                    index = start + 1;
                    continue;
                }
                last = Some(value);
                index = start + consumed;
                if !text.is_char_boundary(index) {
                    index = start + 1;
                }
            }
            _ => index = start + 1,
        }
    }
    match last {
        Some(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string()),
        None => text.to_string(),
    }
}

fn grok_usage_line(data: &Value) -> Option<String> {
    let usage = data.get("usage").filter(|value| py_truthy(Some(value)))?;
    let mut line = format!(
        "[crew] tokens={} turns={}",
        py_display(usage.get("total_tokens")),
        py_display(data.get("num_turns")),
    );
    if let Some(cost) = data.get("total_cost_usd").filter(|value| !value.is_null()) {
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
    fn stop_reason_normalization_and_a_missing_field() {
        let hello = r#"{"text":"hello"}"#;
        let report = assess_grok(false, false, false, false, 0, hello, "");
        assert_eq!(report.code, 0);
        assert_eq!(report.stdout, "hello\n");
        assert!(!report.after_stderr.contains("ended early"));

        for stop in ["end_turn", "EndTurn", "end-turn", "end turn"] {
            let out = format!(r#"{{"text":"hello","stopReason":"{stop}"}}"#);
            assert_eq!(
                assess_grok(false, false, false, false, 0, &out, "").code,
                0,
                "{stop}"
            );
        }

        let cancelled = r#"{"text":"hello","stopReason":"cancelled","sessionId":"s1"}"#;
        let report = assess_grok(false, true, false, true, 0, cancelled, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("SESSION_ID=s1"));
        assert!(report.after_stderr.contains("blocked"));
        assert!(report.after_stderr.contains("shell"));

        let canceled = r#"{"text":"hello","stopReason":"canceled"}"#;
        let report = assess_grok(false, false, false, true, 0, canceled, "");
        assert_eq!(report.code, 125);
        assert!(!report.after_stderr.contains("blocked"));

        let capped = r#"{"text":"hello","stopReason":"max_turns"}"#;
        let report = assess_grok(false, false, false, false, 0, capped, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("--max-turns"));

        let error = r#"{"type":"error","message":"boom"}"#;
        assert_eq!(assess_grok(false, true, false, false, 0, error, "").code, 1);
        assert_eq!(assess_grok(false, true, false, false, 4, error, "").code, 4);
        assert_eq!(assess_grok(false, false, false, false, 0, "", "").code, 0);
        assert_eq!(
            assess_grok(true, true, false, false, 0, cancelled, "").code,
            0
        );
    }

    #[test]
    fn readonly_retries_once_on_sandbox_refusal() {
        let refused = "Error: sandbox could not be applied: /var/run/docker.sock\n";
        assert!(should_retry_readonly(true, false, 1, refused));
        assert!(should_retry_readonly(
            true,
            false,
            2,
            "could not apply the read-only sandbox profile"
        ));
        assert!(!should_retry_readonly(true, false, 0, refused));
        assert!(!should_retry_readonly(true, false, 124, refused));
        assert!(!should_retry_readonly(true, true, 1, refused));
        assert!(!should_retry_readonly(false, false, 1, refused));
        assert_eq!(
            sandbox_detail(refused),
            "Error: sandbox could not be applied: /var/run/docker.sock"
        );

        let mut invocation = bare(Provider::Grok);
        invocation.read_only = true;
        let (first, _) = grok_argv(&invocation, Some("grok-4.7"), "medium", "hello", None, true);
        assert_eq!(flag(&first, "--sandbox"), Some("read-only"));
        assert!(!has(&first, "--tools"));
        let (second, _) = grok_argv(
            &invocation,
            Some("grok-4.7"),
            "medium",
            "hello",
            None,
            false,
        );
        assert!(!has(&second, "--sandbox"));
        assert_eq!(
            flag(&second, "--tools"),
            Some("read_file,grep,list_dir,web_search,web_fetch")
        );
        assert!(has(&second, "--no-subagents"));
        assert_eq!(flag(&second, "--deny"), Some("MCPTool"));

        invocation.no_web = true;
        let (second, _) = grok_argv(&invocation, Some("grok-4.7"), "low", "hello", None, false);
        assert_eq!(flag(&second, "--tools"), Some("read_file,grep,list_dir"));
        assert!(has(&second, "--disable-web-search"));
    }

    #[test]
    fn argv_omits_model_and_prefers_resume() {
        let invocation = bare(Provider::Grok);
        let (argv, note) = grok_argv(&invocation, None, "medium", "hello", None, true);
        assert!(note.is_none());
        assert!(!has(&argv, "-m"));
        assert_eq!(flag(&argv, "--effort"), Some("medium"));
        assert!(has(&argv, "--always-approve"));
        assert!(!has(&argv, "--sandbox"));

        let (argv, note) = grok_argv(&invocation, None, "max", "hello", None, true);
        assert_eq!(flag(&argv, "--effort"), Some("xhigh"));
        assert_eq!(note.as_deref(), Some("[crew] effort max -> xhigh"));

        let (argv, note) = grok_argv(&invocation, Some("grok-4.5"), "max", "hello", None, true);
        assert_eq!(flag(&argv, "--effort"), Some("high"));
        assert_eq!(note.as_deref(), Some("[crew] effort max -> high"));
        assert_eq!(flag(&argv, "-m"), Some("grok-4.5"));

        let mut invocation = bare(Provider::Grok);
        invocation.write = true;
        invocation.resume = Some("sid".to_string());
        invocation.continue_session = true;
        invocation.max_turns = Some(0);
        invocation.cwd = Some("/tmp/work".into());
        let (argv, _) = grok_argv(&invocation, Some("grok-4.7"), "low", "hello", None, true);
        assert_eq!(flag(&argv, "--sandbox"), Some("workspace"));
        assert_eq!(flag(&argv, "--effort"), Some("low"));
        assert!(has(&argv, "--resume"));
        assert!(!has(&argv, "--continue"));
        assert!(!has(&argv, "--max-turns"));
        assert_eq!(flag(&argv, "--cwd"), Some("/tmp/work"));

        invocation.max_turns = Some(3);
        invocation.resume = None;
        let (argv, _) = grok_argv(&invocation, Some("grok-4.7"), "low", "hello", None, true);
        assert_eq!(flag(&argv, "--max-turns"), Some("3"));
        assert!(has(&argv, "--continue"));
    }
}
