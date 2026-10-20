use crate::cli::Invocation;
use crate::error::Error;

/// A missing `stopReason` is success. The skill text is stricter than grok-run,
/// and the script is the behavior to keep. Read-only retries once when stderr
/// says the sandbox profile did not apply.
pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew grok is not implemented yet"))
}
