use std::collections::BTreeSet;
use std::path::Path;

use crate::error::Error;
use crate::review::store;

/// Benchmark context providing prior expectations before weighing review records.
pub const BENCHMARKS: &str = "\
Public benchmarks are mostly vendor-reported and leaderboards disagree, so treat them \
as directional. claude-opus-5-5 has the strongest evidence for bug hunting and code \
review, and at max effort leads WebDev Arena for prototypes, with claude-sonnet-5-5 at \
xhigh close behind. claude-sonnet-5-5 ranks above claude-opus-5-5 on the Vals Index, \
so it is the better value for finishing full products, while claude-fable-5-1 reports \
the best research and reading results. gpt-6.1-sol scores within about a point of \
gpt-6-astra at about a fifth of the price, and gpt-6-luna and claude-haiku-5-5 are the \
cheap picks for quick, narrow edits, claude-haiku-5-5 being the fastest. \
gemini-3.8-flash is fast but verbose, grok-4.7 trails independent indices, and \
gemini-4-argon has no current benchmark data.";

/// Build the prompt instructing the judge to pick exactly one candidate model.
pub fn build_prompt(usage: &str, candidates_json: &str, history_json: &str, task: &str) -> String {
    format!(
        "You choose which model should handle a new subagent task. Everything you need \
is in this prompt: do not use any tools or run any commands. Use the review history \
as evidence. Reply with exactly one model name from the candidate list. \
No explanation, punctuation, or formatting. Public benchmarks below are only \
a prior; first-hand review history outweighs them. Do not pick a model whose \
subscription (claude, codex, antigravity/gemini, grok) is near its limit in \
the usage output below, even if it would otherwise be the best fit.\n\
Public benchmarks: {BENCHMARKS}\n\
Subscription usage:\n{usage}\n\
Candidates: {candidates_json}\nReview history: {history_json}\nNew task: {task}"
    )
}

/// Construct invocation arguments for a review judge.
pub fn judge_args(provider: &str, model: &str) -> Vec<String> {
    vec![
        provider.to_string(),
        "--read-only".to_string(),
        "-m".to_string(),
        model.to_string(),
        "-e".to_string(),
        "high".to_string(),
        "-t".to_string(),
        "300".to_string(),
        "-".to_string(),
    ]
}

/// Validate that a judge's chosen model belongs to the candidate set.
pub fn validate_candidate(answer: &str, candidates: &BTreeSet<String>) -> Result<String, Error> {
    let trimmed = answer.trim();
    if candidates.contains(trimmed) {
        Ok(trimmed.to_string())
    } else {
        Err(Error::new(
            2,
            format!("invalid candidate model: {trimmed:?}"),
        ))
    }
}

/// Query subscription usage from the current binary's `usage` subcommand.
pub fn get_usage_text(exe: &Path) -> String {
    match std::process::Command::new(exe).arg("usage").output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                "usage collection returned empty output".to_string()
            } else {
                text
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                format!("usage collection failed (exit status {})", output.status)
            } else {
                format!("usage collection failed: {stderr}")
            }
        }
        Err(err) => format!("usage collection failed: {err}"),
    }
}

fn random_index(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = [0u8; 1];
        if file.read_exact(&mut buf).is_ok() {
            return (buf[0] as usize) % n;
        }
    }
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as usize)
        % n
}

/// Select the best subagent model for a task using an LLM judge.
pub fn pick(task: &str) -> Result<i32, Error> {
    if task.trim().is_empty() {
        return Err(Error::new(2, "task cannot be empty"));
    }
    let reviews = store::load()?;
    if reviews.is_empty() {
        return Err(Error::new(2, "no reviews yet"));
    }

    let candidates: BTreeSet<String> = reviews.iter().map(|r| r.model.clone()).collect();
    let candidates_vec: Vec<String> = candidates.iter().cloned().collect();
    let candidates_json = serde_json::to_string(&candidates_vec)
        .map_err(|e| Error::new(2, format!("failed to serialize candidates: {e}")))?;
    let history_json = serde_json::to_string(&reviews)
        .map_err(|e| Error::new(2, format!("failed to serialize history: {e}")))?;

    let current_exe = std::env::current_exe()
        .map_err(|e| Error::new(2, format!("failed to get current executable: {e}")))?;
    let usage_text = get_usage_text(&current_exe);
    let prompt = build_prompt(&usage_text, &candidates_json, &history_json, task);

    let judges = [
        ("claude", "haiku"),
        ("gemini", "gemini-3.8-flash"),
        ("codex", "gpt-6-luna"),
    ];

    let start_idx = random_index(judges.len());
    for offset in 0..judges.len() {
        let (provider, model) = judges[(start_idx + offset) % judges.len()];
        let args = judge_args(provider, model);

        let mut cmd = std::process::Command::new(&current_exe);
        cmd.args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(_) => continue,
        };

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            if stdin.write_all(prompt.as_bytes()).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
        }

        let output = match child.wait_with_output() {
            Ok(out) => out,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let answer = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Ok(valid_candidate) = validate_candidate(&answer, &candidates) {
            println!("{valid_candidate}");
            return Ok(0);
        }
    }

    Err(Error::new(2, "no judge returned a valid candidate"))
}
