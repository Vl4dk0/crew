pub mod picker;
pub mod store;

use std::time::Duration;

use crate::error::Error;

const REVIEW_AFTER: Duration = Duration::from_secs(20 * 60);

/// Supported harness categories for filtering reviews.
pub const HARNESSES: [&str; 4] = ["codex", "claude", "agy", "grok"];

pub const CODEX_MODELS: [&str; 9] = [
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-reserve",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

pub const CLAUDE_MODELS: [&str; 4] = [
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
];

pub const AGY_MODELS: [&str; 2] = ["gemini-3.8-flash", "gemini-4-argon"];

pub const GROK_MODELS: [&str; 4] = ["grok-4.7", "grok-4.7-build-fast", "grok-4.6", "grok-4.5"];

pub const EFFORTS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

const FIELDS: [&str; 8] = [
    "when",
    "model",
    "task",
    "what_they_did",
    "duration_s",
    "cost",
    "score",
    "reviewer",
];

/// A note for the orchestrator when a run was long enough to be worth rating.
///
/// Vladko asked for this because a finished long run otherwise disappears
/// without a review. crew does not write the review itself.
pub fn long_run_note(elapsed: Duration, model: &str) -> Option<String> {
    if elapsed <= REVIEW_AFTER {
        return None;
    }
    let minutes = elapsed.as_secs() / 60;
    Some(format!(
        "[crew] run took {minutes}m with {model}. Orchestrator: rate it later with `crew review write`."
    ))
}

/// One row to append to the review log.
pub struct NewReview {
    pub model: String,
    pub task: String,
    pub what: String,
    pub duration: String,
    pub cost: String,
    pub score: String,
    pub reviewer: String,
}

/// Resolve the harness associated with a full model identifier `<model-id>-<effort>`.
pub fn harness_for_model(model: &str) -> Option<&'static str> {
    let (model_id, effort) = model.rsplit_once('-')?;
    if !EFFORTS.contains(&effort) {
        return None;
    }
    if CODEX_MODELS.contains(&model_id) {
        Some("codex")
    } else if CLAUDE_MODELS.contains(&model_id) {
        Some("claude")
    } else if AGY_MODELS.contains(&model_id) {
        Some("agy")
    } else if GROK_MODELS.contains(&model_id) {
        Some("grok")
    } else {
        None
    }
}

/// Ensure a model conforms to `<model-id>-<effort>` using catalog models and recognized efforts.
pub fn validate_model(model: &str) -> Result<(), Error> {
    if harness_for_model(model).is_some() {
        Ok(())
    } else {
        let mut allowed = Vec::new();
        allowed.extend_from_slice(&CODEX_MODELS);
        allowed.extend_from_slice(&CLAUDE_MODELS);
        allowed.extend_from_slice(&AGY_MODELS);
        allowed.extend_from_slice(&GROK_MODELS);
        Err(Error::new(
            2,
            format!(
                "expected model format '<model-id>-<effort>'; effort must be {}; allowed model ids: {}",
                EFFORTS.join("|"),
                allowed.join(", ")
            ),
        ))
    }
}

/// Parse a duration string specified in raw seconds or compounded units (e.g. `1h30m15s`).
pub fn parse_duration(value: &str) -> Result<u64, Error> {
    if value.is_empty() {
        return Err(Error::new(2, "invalid duration: ''"));
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        let secs = value
            .parse::<u64>()
            .map_err(|_| Error::new(2, format!("invalid duration: '{value}'")))?;
        return Ok(secs);
    }

    let mut remaining = value;
    let mut seconds: u64 = 0;
    let mut last_order: i32 = -1;
    let mut seen_any = false;

    while !remaining.is_empty() {
        let digits_len = remaining
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit())
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);

        if digits_len == 0 {
            return Err(Error::new(2, format!("invalid duration: '{value}'")));
        }

        let (digits, rest) = remaining.split_at(digits_len);
        let amount = digits
            .parse::<u64>()
            .map_err(|_| Error::new(2, format!("invalid duration: '{value}'")))?;

        if rest.is_empty() {
            return Err(Error::new(2, format!("invalid duration: '{value}'")));
        }

        let unit = rest.chars().next().unwrap();
        let unit_len = unit.len_utf8();
        remaining = &rest[unit_len..];

        let (order, multiplier) = match unit {
            'h' => (0, 3600),
            'm' => (1, 60),
            's' => (2, 1),
            _ => return Err(Error::new(2, format!("invalid duration: '{value}'"))),
        };

        if order <= last_order {
            return Err(Error::new(2, format!("invalid duration: '{value}'")));
        }
        last_order = order;
        seen_any = true;
        seconds = seconds
            .checked_add(
                amount
                    .checked_mul(multiplier)
                    .ok_or_else(|| Error::new(2, format!("duration overflow: '{value}'")))?,
            )
            .ok_or_else(|| Error::new(2, format!("duration overflow: '{value}'")))?;
    }

    if !seen_any {
        return Err(Error::new(2, format!("invalid duration: '{value}'")));
    }

    Ok(seconds)
}

/// Parse cost representation, converting `nan` or `-` to None and checking for non-negative values.
pub fn parse_cost(value: &str) -> Result<Option<f64>, Error> {
    if value.eq_ignore_ascii_case("nan") || value == "-" {
        return Ok(None);
    }
    let cost = value
        .parse::<f64>()
        .map_err(|_| Error::new(2, format!("invalid cost: '{value}'")))?;
    if !cost.is_finite() || cost < 0.0 {
        return Err(Error::new(2, format!("invalid cost: '{value}'")));
    }
    Ok(Some(cost))
}

/// Parse integer score bounded between 0 and 100 inclusive.
pub fn parse_score(value: &str) -> Result<u8, Error> {
    let score = value
        .parse::<i64>()
        .map_err(|_| Error::new(2, format!("invalid score: '{value}'")))?;
    if !(0..=100).contains(&score) {
        return Err(Error::new(2, format!("invalid score: '{value}'")));
    }
    Ok(score as u8)
}

/// Format reviews into a plain text column table.
pub fn format_table(reviews: &[store::Review]) -> String {
    let mut rows: Vec<[String; 8]> = Vec::with_capacity(reviews.len() + 1);
    rows.push(FIELDS.map(ToString::to_string));

    for r in reviews {
        let cost_str = match r.cost {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        rows.push([
            r.when.clone(),
            r.model.clone(),
            r.task.clone(),
            r.what_they_did.clone(),
            r.duration_s.to_string(),
            cost_str,
            r.score.to_string(),
            r.reviewer.clone(),
        ]);
    }

    let mut widths = [0usize; 8];
    for col in 0..8 {
        widths[col] = rows.iter().map(|row| row[col].len()).max().unwrap_or(0);
    }

    rows.iter()
        .map(|row| {
            row.iter()
                .zip(&widths)
                .map(|(val, width)| format!("{val:<width$}"))
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn write(review: NewReview) -> Result<i32, Error> {
    validate_model(&review.model)?;
    if review.task.trim().is_empty() {
        return Err(Error::new(2, "task cannot be empty"));
    }
    if review.what.trim().is_empty() {
        return Err(Error::new(2, "what_they_did cannot be empty"));
    }
    let duration_s = parse_duration(&review.duration)?;
    let cost = parse_cost(&review.cost)?;
    let score = parse_score(&review.score)?;
    if review.reviewer.trim().is_empty() {
        return Err(Error::new(2, "reviewer cannot be empty"));
    }

    let record = store::Review {
        when: store::now_utc_rfc3339(),
        model: review.model,
        task: review.task,
        what_they_did: review.what,
        duration_s,
        cost,
        score,
        reviewer: review.reviewer,
    };
    store::append(record)?;
    Ok(0)
}

pub fn query(harness: &str) -> Result<i32, Error> {
    if harness != "all" && !HARNESSES.contains(&harness) {
        return Err(Error::new(2, format!("unknown harness: '{harness}'")));
    }
    let reviews = store::load()?;
    let filtered: Vec<store::Review> = reviews
        .into_iter()
        .filter(|r| harness == "all" || harness_for_model(&r.model) == Some(harness))
        .collect();

    println!("{}", format_table(&filtered));
    Ok(0)
}

pub fn pick(task: &str) -> Result<i32, Error> {
    picker::pick(task)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn short_runs_do_not_ask_for_a_review() {
        assert_eq!(long_run_note(Duration::from_secs(19 * 60), "haiku"), None);
    }

    #[test]
    fn long_runs_name_the_model() {
        let note = long_run_note(Duration::from_secs(23 * 60 + 15), "gpt-6-luna").unwrap();
        assert!(note.contains("23m"));
        assert!(note.contains("gpt-6-luna"));
        assert!(note.contains("crew review write"));
    }

    #[test]
    fn parse_duration_valid() {
        assert_eq!(parse_duration("0").unwrap(), 0);
        assert_eq!(parse_duration("120").unwrap(), 120);
        assert_eq!(parse_duration("45s").unwrap(), 45);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration("1h30m15s").unwrap(), 5415);
        assert_eq!(parse_duration("1h15s").unwrap(), 3615);
        assert_eq!(parse_duration("2h5m").unwrap(), 7500);
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("30m1h").is_err());
        assert!(parse_duration("1h1h").is_err());
        assert!(parse_duration("1h 30m").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration("-10").is_err());
    }

    #[test]
    fn parse_cost_valid_and_invalid() {
        assert_eq!(parse_cost("nan").unwrap(), None);
        assert_eq!(parse_cost("NaN").unwrap(), None);
        assert_eq!(parse_cost("-").unwrap(), None);
        assert_eq!(parse_cost("0").unwrap(), Some(0.0));
        assert_eq!(parse_cost("0.05").unwrap(), Some(0.05));
        assert_eq!(parse_cost("12.34").unwrap(), Some(12.34));

        assert!(parse_cost("-1").is_err());
        assert!(parse_cost("inf").is_err());
        assert!(parse_cost("abc").is_err());
    }

    #[test]
    fn parse_score_valid_and_invalid() {
        assert_eq!(parse_score("0").unwrap(), 0);
        assert_eq!(parse_score("72").unwrap(), 72);
        assert_eq!(parse_score("100").unwrap(), 100);

        assert!(parse_score("-1").is_err());
        assert!(parse_score("101").is_err());
        assert!(parse_score("abc").is_err());
    }

    #[test]
    fn model_and_harness_validation() {
        assert_eq!(harness_for_model("gpt-6-sol-high"), Some("codex"));
        assert_eq!(harness_for_model("claude-haiku-5-5-medium"), Some("claude"));
        assert_eq!(harness_for_model("gemini-3.8-flash-low"), Some("agy"));
        assert_eq!(harness_for_model("grok-4.7-max"), Some("grok"));

        assert!(validate_model("gpt-6-sol-high").is_ok());
        assert!(validate_model("unknown-model-high").is_err());
        assert!(validate_model("gpt-6-sol-super").is_err());
        assert!(validate_model("gpt-6-sol").is_err());
    }

    #[test]
    fn store_atomic_replace_and_cap_30() {
        let temp_dir = std::env::temp_dir().join(format!(
            "crew_test_cap30_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = temp_dir.join("reviews.jsonl");

        for i in 1..=35 {
            let record = store::Review {
                when: "2026-10-20T12:00:00Z".to_string(),
                model: "gpt-6-sol-high".to_string(),
                task: format!("task {i}"),
                what_they_did: format!("did {i}"),
                duration_s: i as u64,
                cost: None,
                score: 80,
                reviewer: "test".to_string(),
            };
            store::append_to(&path, record).unwrap();
        }

        let loaded = store::load_from(&path).unwrap();
        assert_eq!(loaded.len(), 30);
        assert_eq!(loaded.first().unwrap().task, "task 6");
        assert_eq!(loaded.last().unwrap().task, "task 35");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn store_rejects_unknown_fields() {
        let line = r#"{"when":"2026-10-20T12:00:00Z","model":"gpt-6-sol-high","task":"task","what_they_did":"did","duration_s":10,"cost":null,"score":80,"reviewer":"test","unexpected_field":"bad"}"#;
        let result: Result<store::Review, _> = serde_json::from_str(line);
        assert!(result.is_err());
    }

    #[test]
    fn validate_candidate_accepts_and_rejects() {
        let mut candidates = BTreeSet::new();
        candidates.insert("gpt-6-sol-high".to_string());
        candidates.insert("gemini-3.8-flash-medium".to_string());

        assert_eq!(
            picker::validate_candidate("  gpt-6-sol-high \n", &candidates).unwrap(),
            "gpt-6-sol-high"
        );
        assert!(picker::validate_candidate("claude-haiku-5-5-high", &candidates).is_err());
    }

    #[test]
    fn judge_argv_construction() {
        let claude = picker::judge_args("claude", "haiku");
        assert_eq!(
            claude,
            vec![
                "claude",
                "--read-only",
                "-m",
                "haiku",
                "-e",
                "high",
                "-t",
                "300",
                "-"
            ]
        );

        let gemini = picker::judge_args("gemini", "gemini-3.8-flash");
        assert_eq!(
            gemini,
            vec![
                "gemini",
                "--read-only",
                "-m",
                "gemini-3.8-flash",
                "-e",
                "high",
                "-t",
                "300",
                "-"
            ]
        );

        let codex = picker::judge_args("codex", "gpt-6-luna");
        assert_eq!(
            codex,
            vec![
                "codex",
                "--read-only",
                "-m",
                "gpt-6-luna",
                "-e",
                "high",
                "-t",
                "300",
                "-"
            ]
        );
    }

    #[test]
    fn prompt_construction() {
        let prompt = picker::build_prompt(
            "usage text here",
            r#"["candidate-1"]"#,
            r#"[{"model":"candidate-1"}]"#,
            "my task description",
        );
        assert!(prompt.contains(picker::BENCHMARKS));
        assert!(prompt.contains("usage text here"));
        assert!(prompt.contains(r#"["candidate-1"]"#));
        assert!(prompt.contains(r#"[{"model":"candidate-1"}]"#));
        assert!(prompt.contains("my task description"));
        assert!(prompt.contains("Reply with exactly one model name"));
    }
}
