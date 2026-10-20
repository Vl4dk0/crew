use crate::error::Error;

/// Print the usagebar layout from the same sources. Never log credential values.
pub fn print_usage() -> Result<i32, Error> {
    Err(Error::new(2, "crew usage is not implemented yet"))
}
