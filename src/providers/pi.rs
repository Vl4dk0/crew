//! pi headless runner.
//!
//! `--mode json` prints JSONL. The first line is the session header
//! (`{"type":"session","id":...}`), before the turn finishes, so a killed
//! run can still be resumed. The answer is the last assistant `message_end`.
//! pi exits 0 even when that message failed, so the stop reason decides.

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;
use crate::tier::{self, PiSettings};

use super::proc::{
    self, KillStyle, Report, StdioKind, looks_like_usage_limit, map_effort, shown, take_prompt,
    usage_limit_line,
};

pub fn run(invocation: &Invocation) -> Result<i32, Error> {
    let prompt = take_prompt(invocation)?.ok_or_else(|| {
        Error::new(
            2,
            "empty prompt (pass a prompt argument or pipe one via stdin)",
        )
    })?;
    let name = invocation.user_agent.as_deref().unwrap_or("pi");
    let settings = tier::pi_settings(
        name,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    let program = match &invocation.user_agent {
        None => vec!["pi".to_string()],
        Some(agent) => {
            let spec = tier::agent(agent)?
                .ok_or_else(|| Error::new(2, format!("unknown agent {agent} in config.toml")))?;
            spec.command
                .iter()
                .map(|part| proc::expand_user(part).to_string_lossy().into_owned())
                .collect()
        }
    };
    execute(invocation, &prompt, &settings, &program, name)
}

pub(crate) fn cli_model(invocation: &Invocation) -> Result<Option<String>, Error> {
    let name = invocation.user_agent.as_deref().unwrap_or("pi");
    let settings = tier::pi_settings(
        name,
        invocation.tier,
        invocation.model.as_deref(),
        invocation.effort.as_deref(),
    )?;
    Ok(settings.model)
}

pub(super) fn execute(
    invocation: &Invocation,
    prompt: &str,
    settings: &PiSettings,
    program: &[String],
    label: &str,
) -> Result<i32, Error> {
    let (tail, note) = pi_tail(
        invocation,
        settings.model.as_deref(),
        settings.thinking.as_deref(),
        prompt,
    );
    let mut argv = program.to_vec();
    argv.extend(tail);
    proc::write_effort_note(&note);
    let timeout = proc::timeout_duration(invocation.timeout_secs)?;
    // `pi` is the message the missing-binary path must print. An agent
    // names itself, which is the word the user typed.
    let mut watch = |line: &str| session_from_line(line);
    let captured = match proc::launch_or_note(
        label,
        proc::capture(
            &argv,
            invocation.cwd.as_deref(),
            timeout,
            StdioKind::Piped,
            KillStyle::TermThenKill,
            &mut watch,
        ),
    ) {
        Ok(captured) => captured,
        Err(code) => return Ok(code),
    };
    let report = assess(label, captured.code, &captured.stdout, &captured.stderr);
    let report = proc::keep_timeout(report, captured.timed_out, invocation.timeout_secs);
    proc::emit_report(&report, captured.session_id.as_deref());
    Ok(report.code)
}

/// Flags after the program name. `pi` and a user agent share this tail.
pub(super) fn pi_tail(
    invocation: &Invocation,
    model: Option<&str>,
    thinking: Option<&str>,
    prompt: &str,
) -> (Vec<String>, Option<String>) {
    let mut argv = vec![
        "-p".to_string(),
        "--mode".to_string(),
        "json".to_string(),
        "--approve".to_string(),
    ];
    if let Some(model) = model {
        proc::push_pair(&mut argv, "--model", model);
    }
    let note = if let Some(thinking) = thinking {
        let (wire, note) = map_effort(Provider::Pi, model, thinking);
        proc::push_pair(&mut argv, "--thinking", &wire);
        note
    } else {
        None
    };
    if let Some(resume) = &invocation.resume {
        proc::push_pair(&mut argv, "--session", resume);
    } else if invocation.continue_session {
        argv.push("--continue".to_string());
    }
    argv.push(prompt.to_string());
    (argv, note)
}

/// Session id on the header line. Later events also carry ids that are not
/// the session, so this does not reuse the generic session-key scan.
pub(super) fn session_from_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

struct Assistant {
    text: String,
    stop: String,
    error: Option<String>,
    usage: Option<Value>,
}

pub(super) fn assess(label: &str, child: i32, out: &str, err: &str) -> Report {
    let (session, assistant, turns) = fold_stream(out);
    let mut after = String::new();
    if let Some(id) = &session {
        after.push_str(&format!("SESSION_ID={id}\n"));
    }
    let Some(assistant) = assistant else {
        let blob = format!("{out}\n{err}");
        if child != 0 && looks_like_usage_limit(&blob) {
            after.push_str(&usage_limit_line(label, &blob));
            return report(75, String::new(), String::new(), after);
        }
        if !err.trim().is_empty() {
            after.push_str(err);
            if !after.ends_with('\n') {
                after.push('\n');
            }
        }
        let code = if child == 0 {
            125
        } else if child >= 128 {
            child
        } else {
            125
        };
        if code == 125 && !after.contains("[crew]") {
            after.push_str(&format!("[crew] error: {label} returned no result\n"));
        }
        return report(code, String::new(), String::new(), after);
    };
    if let Some(message) = &assistant.error {
        if looks_like_usage_limit(message) {
            after.push_str(&usage_limit_line(label, message));
            return report(75, String::new(), String::new(), after);
        }
        after.push_str(&format!("[crew] error: {message}\n"));
        return report(125, String::new(), text_out(&assistant.text), after);
    }
    if assistant.stop != "stop" {
        let stop = if assistant.stop.is_empty() {
            "unknown"
        } else {
            assistant.stop.as_str()
        };
        after.push_str(&format!("[crew] run ended early: stopReason={stop}\n"));
        return report(125, String::new(), text_out(&assistant.text), after);
    }
    if let Some(line) = usage_line(assistant.usage.as_ref(), turns) {
        after.push_str(&line);
        after.push('\n');
    }
    // A signal (job stop, Ctrl-C) keeps its code. Any other non-zero exit
    // is an incomplete run: pi itself often exits 0 on a failed turn.
    let code = if child == 0 || child >= 128 {
        child
    } else {
        125
    };
    if child != 0 && !err.trim().is_empty() {
        after.push_str(err);
    }
    report(code, String::new(), text_out(&assistant.text), after)
}

fn text_out(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!("{}\n", text.trim_end())
    }
}

fn fold_stream(out: &str) -> (Option<String>, Option<Assistant>, u64) {
    let mut session = None;
    let mut assistant = None;
    let mut turns = 0u64;
    for line in out.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") if session.is_none() => {
                session = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
            Some("turn_end") => turns = turns.saturating_add(1),
            Some("message_end") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                assistant = Some(read_assistant(message));
            }
            _ => {}
        }
    }
    (session, assistant, turns)
}

fn read_assistant(message: &Value) -> Assistant {
    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<String>(),
        _ => String::new(),
    };
    let error = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    Assistant {
        text,
        stop: message
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        error,
        usage: message.get("usage").cloned().filter(Value::is_object),
    }
}

/// Same shape as the other providers: counts the stream actually carried,
/// and a missing field is left out rather than printed as None.
fn usage_line(usage: Option<&Value>, turns: u64) -> Option<String> {
    let usage = usage?;
    let mut line = String::from("[crew]");
    if let Some(input) = shown(usage.get("input")) {
        line.push_str(&format!(" in={input}"));
    }
    if let Some(output) = shown(usage.get("output")) {
        line.push_str(&format!(" out={output}"));
    }
    if turns > 0 {
        line.push_str(&format!(" turns={turns}"));
    }
    if let Some(cost) = usage.get("cost").and_then(|cost| shown(cost.get("total"))) {
        line.push_str(&format!(" cost=${cost}"));
    }
    (line != "[crew]").then_some(line)
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
    use crate::providers::proc::bare;

    #[test]
    fn json_stream_and_argv_follow_the_pi_events() {
        let ok = "\
{\"type\":\"session\",\"id\":\"sess-1\"}\n\
{\"type\":\"turn_end\"}\n\
{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"pong\"}],\"stopReason\":\"stop\",\"usage\":{\"input\":3,\"output\":1,\"totalTokens\":4,\"cost\":{\"total\":0}}}}\n";
        let report = assess("pi", 0, ok, "");
        assert_eq!(report.code, 0);
        assert_eq!(report.stdout, "pong\n");
        assert!(report.after_stderr.contains("SESSION_ID=sess-1"));
        assert!(report.after_stderr.contains("in=3"));
        assert!(report.after_stderr.contains("out=1"));
        assert!(report.after_stderr.contains("turns=1"));
        assert!(report.after_stderr.contains("cost=$0"));
        assert!(!report.after_stderr.contains("None"));
        assert_eq!(
            session_from_line(ok.lines().next().unwrap()).as_deref(),
            Some("sess-1")
        );

        let limited = "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[],\"stopReason\":\"error\",\"errorMessage\":\"You've hit your usage limit.\"}}\n";
        assert_eq!(assess("pi", 0, limited, "").code, 75);

        let failed = "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[],\"stopReason\":\"error\",\"errorMessage\":\"Connection error.\"}}\n";
        let report = assess("pi", 0, failed, "");
        assert_eq!(report.code, 125);
        assert!(report.after_stderr.contains("Connection error."));
        assert!(!report.after_stderr.contains("None"));

        let invocation = bare(Provider::Pi);
        let (tail, note) = pi_tail(&invocation, None, None, "pong");
        assert!(note.is_none());
        assert_eq!(tail, vec!["-p", "--mode", "json", "--approve", "pong"]);

        let mut with_flags = invocation;
        with_flags.resume = Some("sess-1".to_string());
        let (tail, note) = pi_tail(&with_flags, Some("provider/model"), Some("max"), "pong");
        assert!(note.is_none());
        assert_eq!(
            tail,
            vec![
                "-p",
                "--mode",
                "json",
                "--approve",
                "--model",
                "provider/model",
                "--thinking",
                "max",
                "--session",
                "sess-1",
                "pong",
            ]
        );

        let mut agent = vec!["tool".to_string(), "--fixed".to_string()];
        let (tail, _) = pi_tail(&bare(Provider::Pi), Some("model-c"), Some("high"), "pong");
        agent.extend(tail);
        assert_eq!(
            agent,
            vec![
                "tool",
                "--fixed",
                "-p",
                "--mode",
                "json",
                "--approve",
                "--model",
                "model-c",
                "--thinking",
                "high",
                "pong",
            ]
        );
    }
}
