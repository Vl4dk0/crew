use std::time::Duration;

use crate::error::Error;

const REVIEW_AFTER: Duration = Duration::from_secs(20 * 60);

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

pub fn write(_review: NewReview) -> Result<i32, Error> {
    Err(Error::new(2, "crew review write is not implemented yet"))
}

pub fn query(_harness: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew review query is not implemented yet"))
}

pub fn pick(_task: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew review pick is not implemented yet"))
}

#[cfg(test)]
mod tests {
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
}
