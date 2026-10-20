use crate::cli::Invocation;
use crate::error::Error;

/// User-facing `max` is sent as `ultra`, except `gpt-6-luna`, which has no
/// `ultra` and is sent `max`. A `turn.failed` anywhere fails the run.
pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew codex is not implemented yet"))
}
