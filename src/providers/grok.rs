//! Grok headless runner.
//!
//! A missing `stopReason` is a finished turn.

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier;

use super::proc::{
    self, KillStyle, Report, StdioKind, USAGE_LIMIT_EXIT, code_or_one, format_secs,
    looks_like_usage_limit, map_effort, py_display, py_truthy, take_prompt, usage_limit_line,
};

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
    let (argv, note) = grok_argv(
        invocation,
        resolved.model.as_deref(),
        &resolved.effort,
        &prompt,
        schema.as_deref(),
    );
    proc::write_effort_note(&note);
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    let captured = match proc::launch_or_note(proc::capture(
        &argv,
        None,
        timeout,
        StdioKind::Piped,
        KillStyle::TermThenKill,
        true,
    )) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    if captured.timed_out {
        proc::write_stderr_line(&format!(
            "[crew] timed out after {}s",
            format_secs(invocation.timeout_secs)
        ));
        return Ok(124);
    }
    let schema_set = schema.as_ref().is_some_and(|text| !text.is_empty());
    let report = assess_grok(
        invocation.raw,
        schema_set,
        captured.code,
        &captured.stdout,
        &captured.stderr,
    );
    proc::emit_report(&report);
    Ok(report.code)
}

pub(super) fn grok_argv(
    invocation: &Invocation,
    model: Option<&str>,
    effort: &str,
    prompt: &str,
    schema: Option<&str>,
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

pub(super) fn assess_grok(raw: bool, schema: bool, child: i32, out: &str, err: &str) -> Report {
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
                if child != 0 && looks_like_usage_limit(out) {
                    return report(
                        USAGE_LIMIT_EXIT,
                        String::new(),
                        String::new(),
                        usage_limit_line("grok", out),
                    );
                }
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
        if looks_like_usage_limit(&message) {
            return report(
                USAGE_LIMIT_EXIT,
                String::new(),
                String::new(),
                usage_limit_line("grok", &message),
            );
        }
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
    if let Some(id) = data.get("sessionId").and_then(Value::as_str)
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
        let hint = early_stop_hint(&stop);
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

fn early_stop_hint(stop: &str) -> &'static str {
    let key = norm_stop(stop);
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
        let report = assess_grok(false, false, 0, hello, "");
        assert_eq!(report.code, 0);
        assert_eq!(report.stdout, "hello\n");
        assert!(!report.after_stderr.contains("ended early"));

        for stop in ["end_turn", "EndTurn", "end-turn", "end turn"] {
            let out = format!(r#"{{"text":"hello","stopReason":"{stop}"}}"#);
            assert_eq!(assess_grok(false, false, 0, &out, "").code, 0, "{stop}");
        }

        let cancelled = r#"{"text":"hello","stopReason":"cancelled","sessionId":"s1"}"#;
        let report = assess_grok(false, false, 0, cancelled, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("SESSION_ID=s1"));
        assert!(report.after_stderr.contains("blocked"));

        let canceled = r#"{"text":"hello","stopReason":"canceled"}"#;
        let report = assess_grok(false, false, 0, canceled, "");
        assert_eq!(report.code, 125);
        assert!(!report.after_stderr.contains("blocked"));

        let capped = r#"{"text":"hello","stopReason":"max_turns"}"#;
        let report = assess_grok(false, false, 0, capped, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("--max-turns"));

        let error = r#"{"type":"error","message":"boom"}"#;
        assert_eq!(assess_grok(false, false, 0, error, "").code, 1);
        assert_eq!(assess_grok(false, false, 4, error, "").code, 4);
        assert_eq!(assess_grok(false, false, 0, "", "").code, 0);
        assert_eq!(assess_grok(true, false, 0, cancelled, "").code, 0);
    }

    #[test]
    fn argv_omits_model_and_prefers_resume() {
        let invocation = bare(Provider::Grok);
        let (argv, note) = grok_argv(&invocation, None, "medium", "hello", None);
        assert!(note.is_none());
        assert!(!has(&argv, "-m"));
        assert_eq!(flag(&argv, "--effort"), Some("medium"));
        assert!(has(&argv, "--always-approve"));
        assert!(!has(&argv, "--sandbox"));

        let (argv, note) = grok_argv(&invocation, None, "max", "hello", None);
        assert_eq!(flag(&argv, "--effort"), Some("xhigh"));
        assert_eq!(note.as_deref(), Some("[crew] effort max -> xhigh"));

        let (argv, note) = grok_argv(&invocation, Some("grok-4.5"), "max", "hello", None);
        assert_eq!(flag(&argv, "--effort"), Some("high"));
        assert_eq!(note.as_deref(), Some("[crew] effort max -> high"));
        assert_eq!(flag(&argv, "-m"), Some("grok-4.5"));

        let mut invocation = bare(Provider::Grok);
        invocation.resume = Some("sid".to_string());
        invocation.continue_session = true;
        invocation.max_turns = Some(0);
        invocation.cwd = Some("/tmp/work".into());
        let (argv, _) = grok_argv(&invocation, Some("grok-4.7"), "low", "hello", None);
        assert_eq!(flag(&argv, "--effort"), Some("low"));
        assert!(has(&argv, "--resume"));
        assert!(!has(&argv, "--continue"));
        assert!(!has(&argv, "--max-turns"));
        assert_eq!(flag(&argv, "--cwd"), Some("/tmp/work"));

        invocation.max_turns = Some(3);
        invocation.resume = None;
        let (argv, _) = grok_argv(&invocation, Some("grok-4.7"), "low", "hello", None);
        assert_eq!(flag(&argv, "--max-turns"), Some("3"));
        assert!(has(&argv, "--continue"));
    }
}
