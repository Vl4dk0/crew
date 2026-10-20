use crate::cli::Invocation;
use crate::error::Error;

/// Gemini bakes `flash` and `argon` effort into the model slug and must not
/// also pass `--effort`. A slug that already ends in `-low`, `-medium`, or
/// `-high` is sent as `--model` alone.
pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew gemini is not implemented yet"))
}
