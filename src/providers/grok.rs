//! Grok headless runner.
//!
//! streaming-messages-json prints the session id on the init line, before the
//! turn finishes, so a killed run can still be resumed. A missing `stopReason`
//! is a finished turn.

use serde_json::Value;

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
    let captured = match proc::launch_or_note(
        "grok",
        proc::capture(
            &argv,
            None,
            timeout,
            StdioKind::Piped,
            KillStyle::TermThenKill,
            &mut watch,
        ),
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    let schema_set = schema.as_ref().is_some_and(|text| !text.is_empty());
    let report = assess_grok(
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
    Ok(grok_argv(
        invocation,
        resolved.model.as_deref(),
        &resolved.effort,
        prompt,
        schema,
    ))
}

pub(crate) fn cli_model(invocation: &Invocation) -> Result<Option<String>, Error> {
    let (argv, _) = plan(invocation, "", None)?;
    Ok(proc::model_arg(&argv))
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
        "streaming-messages-json".to_string(),
        "--include-partial-messages".to_string(),
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
        if let Some(id) = first_session(out) {
            after.push_str(&format!("SESSION_ID={id}\n"));
        }
        return report(child, String::new(), out.to_string(), after);
    }
    let folded;
    let out = if let Some(object) = fold_grok_stream(out) {
        folded = object;
        folded.as_str()
    } else {
        out
    };
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

/// NDJSON from streaming-messages-json, reduced to the single object
/// `assess_grok` already understands. A finished `--output-format json`
/// document is left alone.
fn first_session(out: &str) -> Option<String> {
    proc::session_from_line(out).or_else(|| out.lines().find_map(proc::session_from_line))
}

fn fold_grok_stream(out: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<Value>(out.trim())
        && value.is_object()
        && !is_live_grok_event(&value)
    {
        return None;
    }
    let mut events = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(out.trim())
        && value.is_object()
    {
        events.push(value);
    } else {
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
    }
    if events.is_empty() {
        return None;
    }
    let mut session_id = None;
    let mut result = None;
    let mut error = None;
    let mut deltas = String::new();
    let mut assistant = String::new();
    let mut saw_delta = false;
    for event in &events {
        if session_id.is_none() {
            session_id = proc::session_from_line(&event.to_string());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("result") => result = Some(event),
            Some("error") => error = Some(event),
            Some("assistant") if !saw_delta => append_grok_text(event, &mut assistant),
            Some("text" | "thought") => {
                if let Some(text) = event.get("data").and_then(Value::as_str) {
                    deltas.push_str(text);
                    saw_delta = true;
                }
            }
            Some("stream_event" | "content_block_delta" | "message_delta")
                if append_grok_delta(event, &mut deltas) =>
            {
                saw_delta = true;
            }
            _ => {}
        }
    }
    let mut object = serde_json::Map::new();
    if let Some(id) = session_id {
        object.insert("sessionId".to_string(), Value::String(id));
    }
    if let Some(result) = result {
        if let Some(text) = result.get("result").and_then(Value::as_str) {
            object.insert("text".to_string(), Value::String(text.to_string()));
        }
        if let Some(stop) = result.get("stop_reason").and_then(Value::as_str) {
            object.insert("stopReason".to_string(), Value::String(stop.to_string()));
        }
        if let Some(usage) = result.get("usage") {
            object.insert("usage".to_string(), usage.clone());
        }
        if let Some(turns) = result.get("num_turns") {
            object.insert("num_turns".to_string(), turns.clone());
        }
        if let Some(cost) = result.get("total_cost_usd") {
            object.insert("total_cost_usd".to_string(), cost.clone());
        }
        if let Some(structured) = result.get("structured_output") {
            object.insert("structuredOutput".to_string(), structured.clone());
        }
        if result.get("subtype").and_then(Value::as_str) == Some("error_max_turns") {
            object.insert(
                "stopReason".to_string(),
                Value::String("max_turns".to_string()),
            );
        }
    } else if let Some(error) = error {
        object.insert("type".to_string(), Value::String("error".to_string()));
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        object.insert("message".to_string(), Value::String(message));
    } else {
        let text = if saw_delta { deltas } else { assistant };
        object.insert("text".to_string(), Value::String(text));
    }
    serde_json::to_string(&Value::Object(object)).ok()
}

fn is_live_grok_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "system"
                | "assistant"
                | "user"
                | "stream_event"
                | "text"
                | "thought"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_start"
                | "message_delta"
                | "message_stop"
        )
    )
}

fn append_grok_text(event: &Value, buf: &mut String) {
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

fn append_grok_delta(event: &Value, buf: &mut String) -> bool {
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

/// `streaming-messages-json` has no `total_tokens`. The json format's total is
/// input plus both cache buckets plus output. Reasoning is already inside output.
fn grok_token_total(usage: &Value) -> Option<u64> {
    if let Some(total) = count_field(usage.get("total_tokens")) {
        return Some(total);
    }
    let mut sum = 0u64;
    let mut any = false;
    for key in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ] {
        let Some(count) = count_field(usage.get(key)) else {
            continue;
        };
        sum = sum.saturating_add(count);
        any = true;
    }
    any.then_some(sum)
}

fn count_field(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(count) = value.as_u64() {
        return Some(count);
    }
    let count = value.as_i64()?;
    u64::try_from(count).ok()
}

fn grok_usage_line(data: &Value) -> Option<String> {
    let tokens = data
        .get("usage")
        .filter(|value| py_truthy(Some(value)))
        .and_then(grok_token_total);
    let turns = shown(data.get("num_turns"));
    let cost = shown(data.get("total_cost_usd"));
    if tokens.is_none() && turns.is_none() && cost.is_none() {
        return None;
    }
    let mut line = String::from("[crew]");
    if let Some(tokens) = tokens {
        line.push_str(&format!(" tokens={tokens}"));
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
    fn stream_keeps_the_session_and_partial_text() {
        let out = "\
{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n\
{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm \"}}}\n\
{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}}\n";
        let report = assess_grok(false, false, 143, out, "");
        assert!(report.after_stderr.contains("SESSION_ID=s1"));
        assert_eq!(report.stdout, "hmm hello\n");
        let report = proc::keep_timeout(report, true, 20.0);
        assert_eq!(report.code, 124);
        assert!(report.after_stderr.contains("timed out after 20s"));
        assert_eq!(report.stdout, "hmm hello\n");
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

    #[test]
    fn stream_usage_reports_the_token_total() {
        let input = 10u64;
        let output = 4u64;
        let cache_read = 6u64;
        let cache_write = 1u64;
        let reasoning = 3u64;
        let turns = 2u64;
        let cost = "0.5";
        let stream = format!(
            r#"{{"type":"system","subtype":"init","session_id":"s1"}}
{{"type":"result","subtype":"success","num_turns":{turns},"result":"pong","stop_reason":"end_turn","total_cost_usd":{cost},"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_write},"reasoning_tokens":{reasoning}}}}}
"#
        );
        let report = assess_grok(false, false, 0, &stream, "");
        // Reasoning is already inside output, same as the json format's total_tokens.
        let total = input + cache_read + cache_write + output;
        let line = format!("[crew] tokens={total} turns={turns} cost=${cost}");
        assert!(
            report.after_stderr.contains(&line),
            "{}",
            report.after_stderr
        );
        assert!(!report.after_stderr.contains("None"));

        let explicit = input + output + 50;
        let legacy = format!(
            r#"{{"text":"pong","num_turns":{turns},"total_cost_usd":{cost},"usage":{{"input_tokens":{input},"output_tokens":{output},"total_tokens":{explicit}}}}}"#
        );
        let report = assess_grok(false, false, 0, &legacy, "");
        assert!(report.after_stderr.contains(&format!(
            "[crew] tokens={explicit} turns={turns} cost=${cost}"
        )));

        let no_counts = format!(
            r#"{{"type":"result","result":"pong","stop_reason":"end_turn","num_turns":{turns},"total_cost_usd":{cost},"usage":{{"server_tool_use":{{"web_search_requests":0}}}}}}"#
        );
        let report = assess_grok(false, false, 0, &no_counts, "");
        assert!(
            report
                .after_stderr
                .contains(&format!("[crew] turns={turns} cost=${cost}"))
        );
        assert!(!report.after_stderr.contains("tokens="));
        assert!(!report.after_stderr.contains("None"));

        let no_turns = format!(
            r#"{{"text":"pong","total_cost_usd":{cost},"usage":{{"total_tokens":{explicit}}}}}"#
        );
        let report = assess_grok(false, false, 0, &no_turns, "");
        assert!(
            report
                .after_stderr
                .contains(&format!("[crew] tokens={explicit} cost=${cost}"))
        );
        assert!(!report.after_stderr.contains("turns="));
        assert!(!report.after_stderr.contains("None"));
    }
}
