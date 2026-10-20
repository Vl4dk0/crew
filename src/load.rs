use crate::error::Error;

/// Print the same lines as subagentcounter, then a `crew:` line.
pub fn print_load() -> Result<i32, Error> {
    Err(Error::new(2, "crew load is not implemented yet"))
}
