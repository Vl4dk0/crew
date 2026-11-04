//! Live lines in `runs/<name>/steps` for `crew job peek`.
//!
//! The wrapper already reads each stdout line. A step is a tool starting or
//! the answer. Token deltas are not steps. A foreground run has no
//! `CREW_JOB`, so it leaves no file.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

use serde_json::Value;

const CLIP: usize = 80;

struct Step {
    /// The same call is announced twice: once at start, again in the full message.
    id: Option<String>,
    body: String,
}

static SEEN: Mutex<(Vec<String>, String)> = Mutex::new((Vec::new(), String::new()));

pub fn record(lines: &[String]) {
    let Ok(name) = std::env::var("CREW_JOB") else {
        return;
    };
    if name.is_empty() {
        return;
    }
    let mut buf = String::new();
    for line in lines {
        for step in bodies(line) {
            if !fresh(&step) {
                continue;
            }
            buf.push_str(&clock());
            buf.push_str("  ");
            buf.push_str(&step.body);
            buf.push('\n');
        }
    }
    if buf.is_empty() {
        return;
    }
    // Peek is a side channel. A bad steps file must not change the run's exit.
    let path = crate::job::store::run_dir(&name).join("steps");
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    if let Ok(mut file) = opts.open(path) {
        let _ = file.write_all(buf.as_bytes());
    }
}

fn fresh(step: &Step) -> bool {
    let mut seen = SEEN.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(id) = &step.id {
        if seen.0.iter().any(|seen_id| seen_id == id) {
            return false;
        }
        if seen.0.len() == 64 {
            seen.0.remove(0);
        }
        seen.0.push(id.clone());
    }
    if step.body.starts_with("say ") {
        if seen.1 == step.body {
            return false;
        }
        seen.1.clone_from(&step.body);
    }
    true
}

fn bodies(line: &str) -> Vec<Step> {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let item = value.pointer("/params/item");
        match method {
            "item/started" => push_tool(item, &mut out),
            "item/completed" => push_codex_answer(item, &mut out),
            _ => {}
        }
        return out;
    }
    // `text`, `thought`, `stream_event`, and `message_update` are token deltas.
    match value.get("type").and_then(Value::as_str) {
        Some("item.started") => push_tool(value.get("item"), &mut out),
        Some("item.completed") => push_codex_answer(value.get("item"), &mut out),
        Some("assistant") => {
            if let Some(message) = value.get("message") {
                push_assistant(message, &mut out);
            }
        }
        Some("result") => push_say(value.get("result").and_then(Value::as_str), true, &mut out),
        Some("tool_execution_start") => out.push(Step {
            id: text(&value, "toolCallId"),
            body: tool_body(
                value
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool"),
                &value.get("args").map(short_arg).unwrap_or_default(),
            ),
        }),
        Some("message_end") => {
            let message = value.get("message");
            if message
                .and_then(|msg| msg.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
            {
                push_say(message.map(joined_text).as_deref(), true, &mut out);
            }
        }
        Some("tool_call" | "function_call") => push_tool(Some(&value), &mut out),
        _ => {}
    }
    out
}

fn push_assistant(message: &Value, out: &mut Vec<Step>) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut aside = String::new();
    let mut saw_tool = false;
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            kind,
            "tool_use" | "tool_call" | "function_call" | "toolCall" | "functionCall"
        ) {
            saw_tool = true;
            push_tool(Some(block), out);
        } else if kind == "text" {
            aside.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""));
        }
    }
    // A short remark next to a tool. The result event is the answer itself.
    if saw_tool {
        push_say(Some(&aside), false, out);
    }
}

fn push_codex_answer(item: Option<&Value>, out: &mut Vec<Step>) {
    let Some(item) = item else {
        return;
    };
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(kind, "agentMessage" | "agent_message") {
        return;
    }
    let commentary = item.get("phase").and_then(Value::as_str) == Some("commentary");
    push_say(item.get("text").and_then(Value::as_str), !commentary, out);
}

fn push_tool(item: Option<&Value>, out: &mut Vec<Step>) {
    let Some(step) = item.and_then(tool_step) else {
        return;
    };
    out.push(step);
}

fn tool_step(item: &Value) -> Option<Step> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(
        kind,
        "agentMessage"
            | "agent_message"
            | "reasoning"
            | "plan"
            | "userMessage"
            | "user_message"
            | "todoList"
            | "todo_list"
            | "text"
            | "thinking"
    ) {
        return None;
    }
    let id = text(item, "id");
    let (name, arg) = if kind.contains("command") || kind.contains("shell") || kind == "exec" {
        ("command".to_string(), arg_at(item, &["command"]))
    } else if kind.contains("web") {
        ("web_search".to_string(), arg_at(item, &["query"]))
    } else if kind.contains("file") {
        (
            "file_change".to_string(),
            item.pointer("/changes/0/path")
                .or_else(|| item.get("path"))
                .map(short_arg)
                .unwrap_or_default(),
        )
    } else if kind.contains("mcp") {
        (
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string(),
            arg_at(item, &["arguments", "args"]),
        )
    } else {
        let name = item
            .get("name")
            .or_else(|| item.get("toolName"))
            .and_then(Value::as_str)?;
        (
            name.to_string(),
            arg_at(item, &["input", "arguments", "args"]),
        )
    };
    Some(Step {
        id,
        body: tool_body(&name, &arg),
    })
}

fn joined_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// `force` is the answer, clipped so a peek line stays one line.
/// A remark is kept only when the whole remark already fits.
fn push_say(text: Option<&str>, force: bool, out: &mut Vec<Step>) {
    let flat = text
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.is_empty() || (!force && flat.chars().count() > CLIP) {
        return;
    }
    let text: String = flat.chars().take(CLIP).collect();
    out.push(Step {
        id: None,
        body: format!("say  {text}"),
    });
}

fn tool_body(name: &str, arg: &str) -> String {
    if arg.is_empty() {
        format!("tool  {name}")
    } else {
        format!("tool  {name}  {arg}")
    }
}

fn arg_at(item: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| item.get(*key))
        .map(short_arg)
        .unwrap_or_default()
}

fn short_arg(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return clip(text);
    }
    let Some(obj) = value.as_object() else {
        return String::new();
    };
    for key in [
        "command",
        "cmd",
        "path",
        "file_path",
        "query",
        "pattern",
        "url",
        "prompt",
    ] {
        if let Some(text) = obj.get(key).and_then(Value::as_str) {
            return clip(text);
        }
    }
    obj.values()
        .find_map(Value::as_str)
        .map(clip)
        .unwrap_or_default()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn clip(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(CLIP)
        .collect()
}

fn clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `tm` is a writable libc::tm. A null return leaves it zeroed.
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_line_names_the_tool_and_the_answer() {
        let tool = bodies(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo one"}}]}}"#,
        );
        assert_eq!(tool.len(), 1);
        assert_eq!(tool[0].body, "tool  Bash  echo one");
        assert!(
            bodies(
                r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"tok"}}}"#
            )
            .is_empty()
        );
        let say = bodies(r#"{"type":"result","result":"pong"}"#);
        assert_eq!(say[0].body, "say  pong");
        let line = format!("{}  {}\n", clock(), say[0].body);
        assert!(line.ends_with("  say  pong\n"));
        assert_eq!(line.as_bytes()[2], b':');
        assert_eq!(line.as_bytes()[5], b':');
    }
}
