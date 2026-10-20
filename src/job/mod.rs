use crate::error::Error;
use crate::id::Provider;

/// A job the supervisor has not started yet.
///
/// `trailing` is the provider argv (`--read-only`, the prompt, …). The CLI
/// already checked that it parses. The supervisor re-execs `crew <provider>`
/// with those words so a background job cannot drift from a foreground one.
pub struct NewJob {
    pub name: String,
    pub provider: Provider,
    pub note: Option<String>,
    pub trailing: Vec<String>,
}

pub fn run(_job: NewJob) -> Result<i32, Error> {
    Err(Error::new(2, "crew job run is not implemented yet"))
}

pub fn ls(_json: bool) -> Result<i32, Error> {
    Err(Error::new(2, "crew job ls is not implemented yet"))
}

pub fn show(_name: &str, _json: bool) -> Result<i32, Error> {
    Err(Error::new(2, "crew job show is not implemented yet"))
}

pub fn result(_name: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew job result is not implemented yet"))
}

pub fn log(_name: &str, _lines: usize) -> Result<i32, Error> {
    Err(Error::new(2, "crew job log is not implemented yet"))
}

pub fn wait(_names: &[String], _any: bool, _timeout_secs: Option<u64>) -> Result<i32, Error> {
    Err(Error::new(2, "crew job wait is not implemented yet"))
}

pub fn resume(_name: &str, _prompt: Option<String>) -> Result<i32, Error> {
    Err(Error::new(2, "crew job resume is not implemented yet"))
}

pub fn stop(_name: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew job stop is not implemented yet"))
}

pub fn supervise(_name: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew job supervisor is not implemented yet"))
}
