//! Terminal for `agy -p`.
//!
//! Antigravity drops stdout when its output is not a terminal. The parent
//! only reads the master side. The prompt stays an argv element.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

pub(super) struct PtyPair {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

pub(super) fn open_pty() -> io::Result<PtyPair> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty writes two fresh fds into the out-params. On failure
    // neither fd is owned by us.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded and transferred these fds to the caller.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    Ok(PtyPair { master, slave })
}

/// `agy-run`'s ANSI pattern: CSI, OSC terminated by BEL, and the small charset
/// selector. Anything else, including a bare ESC, stays in the text.
pub(super) fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            let Some(ch) = input[index..].chars().next() else {
                break;
            };
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        if let Some(len) = ansi_sequence_len(&bytes[index..]) {
            index += len;
        } else {
            out.push('\u{1b}');
            index += 1;
        }
    }
    out
}

fn ansi_sequence_len(bytes: &[u8]) -> Option<usize> {
    if bytes.first() != Some(&0x1b) || bytes.len() < 2 {
        return None;
    }
    match bytes[1] {
        b'[' => csi_len(bytes),
        b']' => osc_len(bytes),
        b'(' | b')' => charset_len(bytes),
        _ => None,
    }
}

fn csi_len(bytes: &[u8]) -> Option<usize> {
    let mut index = 2;
    while index < bytes.len() && matches!(bytes[index], b'0'..=b'9' | b';' | b'?') {
        index += 1;
    }
    if index < bytes.len() && bytes[index].is_ascii_alphabetic() {
        Some(index + 1)
    } else {
        None
    }
}

fn osc_len(bytes: &[u8]) -> Option<usize> {
    let mut index = 2;
    while index < bytes.len() && bytes[index] != 0x07 {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == 0x07 {
        Some(index + 1)
    } else {
        None
    }
}

fn charset_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() >= 3 && matches!(bytes[2], b'A' | b'B' | b'0' | b'1' | b'2') {
        Some(3)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_matches_the_agy_pattern() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}x"), "x");
        assert_eq!(strip_ansi("\u{1b}(Bplain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[?25l"), "");
        assert_eq!(strip_ansi("\u{1b}hi"), "\u{1b}hi");
    }
}
