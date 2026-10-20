use crate::cli::Invocation;
use crate::error::Error;

/// `ultra` is rewritten to `max` only when the model string is exactly
/// `gpt-6-luna`. A `turn.failed` anywhere fails the run.
pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew codex is not implemented yet"))
}
