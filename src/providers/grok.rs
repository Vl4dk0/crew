use crate::cli::Invocation;
use crate::error::Error;

/// User-facing `max` is sent as `xhigh` (`high` on `grok-4.5`). A missing
/// `stopReason` is success. Read-only retries once when stderr says the
/// sandbox profile did not apply.
pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew grok is not implemented yet"))
}
