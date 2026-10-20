use std::fmt;

/// A failure that already knows the process exit code.
///
/// The CLI has a handful of codes (2 for usage, 124/125/127 for runs). Keeping
/// the code on the error stops callers from inventing a second mapping.
#[derive(Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
