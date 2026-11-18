use std::io::{self, Write};

/// Write `bytes` to stdout without panicking on `BrokenPipe`.
/// Returns `false` when stdout is closed (e.g. downstream `head` exited early).
pub fn write_stdout(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let mut out = io::stdout().lock();
    out.write_all(bytes).and_then(|_| out.flush()).is_ok()
}

/// Write `bytes` to stderr without panicking on `BrokenPipe`.
pub fn write_stderr(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut err = io::stderr().lock();
    let _ = err.write_all(bytes).and_then(|_| err.flush());
}

pub fn stdout_str(text: &str) -> bool {
    write_stdout(text.as_bytes())
}

pub fn stdout_line(text: &str) -> bool {
    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.extend_from_slice(text.as_bytes());
    buf.push(b'\n');
    write_stdout(&buf)
}

pub fn stderr_str(text: &str) {
    write_stderr(text.as_bytes());
}

pub fn stderr_line(text: &str) {
    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.extend_from_slice(text.as_bytes());
    buf.push(b'\n');
    write_stderr(&buf);
}

#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::stdout_str(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! outln {
    () => {
        $crate::out::stdout_str("\n")
    };
    ($($arg:tt)*) => {
        $crate::out::stdout_line(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::out::stderr_str(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! errln {
    () => {
        $crate::out::stderr_str("\n")
    };
    ($($arg:tt)*) => {
        $crate::out::stderr_line(&format!($($arg)*))
    };
}
