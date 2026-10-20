use crate::cli::Invocation;
use crate::error::Error;

pub fn run(_invocation: &Invocation) -> Result<i32, Error> {
    Err(Error::new(2, "crew claude is not implemented yet"))
}
