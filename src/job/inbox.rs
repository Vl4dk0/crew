//! Append-only job inbox. Queue and steer are lines in the job directory,
//! not a socket: the supervisor and the running wrapper share the file.
//!
//! A delivery is a second line. The original line stays, so a reader can
//! rebuild the list after a crash without rewriting history.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::job::models::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Queue,
    Steer,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub n: u64,
    pub kind: Kind,
    pub text: String,
    pub at: String,
    pub delivered_at: Option<String>,
}

/// The wrapper of a job reads this. Foreground runs do not set it, so they
/// keep the one-shot CLI invocation.
pub fn job_name() -> Option<String> {
    std::env::var("CREW_JOB")
        .ok()
        .filter(|name| !name.is_empty())
}

/// Running accepts a message. Lost is exit 3. Anything finished is exit 2.
pub fn require_running(status: Status) -> Result<(), Error> {
    match status {
        Status::Running => Ok(()),
        Status::Lost => Err(Error::new(3, "lost job")),
        _ => Err(Error::new(2, "job is not running")),
    }
}

pub fn append(dir: &Path, kind: Kind, text: &str) -> Result<Item, Error> {
    with_inbox(dir, |file| {
        let items = read_items(file)?;
        let n = items
            .last()
            .map(|item| item.n.saturating_add(1))
            .unwrap_or(1);
        let at = super::current_iso();
        let line = serde_json::json!({
            "n": n,
            "kind": kind.as_str(),
            "text": text,
            "at": at,
        });
        writeln!(file, "{line}").map_err(|err| Error::new(2, format!("inbox write: {err}")))?;
        Ok(Item {
            n,
            kind,
            text: text.to_string(),
            at,
            delivered_at: None,
        })
    })
}

pub fn pending(dir: &Path) -> Result<Vec<Item>, Error> {
    if !inbox_path(dir).is_file() {
        return Ok(Vec::new());
    }
    with_inbox(dir, |file| {
        Ok(read_items(file)?
            .into_iter()
            .filter(|item| item.delivered_at.is_none())
            .collect())
    })
}

pub fn delivered(dir: &Path) -> Result<Vec<Item>, Error> {
    if !inbox_path(dir).is_file() {
        return Ok(Vec::new());
    }
    with_inbox(dir, |file| {
        Ok(read_items(file)?
            .into_iter()
            .filter(|item| item.delivered_at.is_some())
            .collect())
    })
}

pub fn mark_delivered(dir: &Path, n: u64) -> Result<(), Error> {
    with_inbox(dir, |file| {
        let at = super::current_iso();
        let line = serde_json::json!({ "n": n, "delivered": at });
        writeln!(file, "{line}").map_err(|err| Error::new(2, format!("inbox write: {err}")))?;
        Ok(())
    })
}

fn inbox_path(dir: &Path) -> PathBuf {
    dir.join("inbox.jsonl")
}

fn with_inbox<T>(dir: &Path, body: impl FnOnce(&mut File) -> Result<T, Error>) -> Result<T, Error> {
    fs::create_dir_all(dir).map_err(|err| Error::new(2, format!("inbox dir: {err}")))?;
    let lock_path = dir.join("inbox.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|err| Error::new(2, format!("inbox lock: {err}")))?;
    let fd = lock.as_raw_fd();
    // SAFETY: flock only serializes writers of this one file. The guard unlocks.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if rc != 0 {
        return Err(Error::new(2, "failed to lock inbox"));
    }
    let _guard = LockGuard(lock);
    let mut file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(inbox_path(dir))
        .map_err(|err| Error::new(2, format!("inbox open: {err}")))?;
    body(&mut file)
}

struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        // SAFETY: this fd is the lock file we opened. Unlock is idempotent.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn read_items(file: &mut File) -> Result<Vec<Item>, Error> {
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| Error::new(2, format!("inbox read: {err}")))?;
    // The next append must stay at the end. read_to_string leaves the cursor there
    // only on some platforms; seek so a later writeln cannot land in the middle.
    use std::io::Seek;
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|err| Error::new(2, format!("inbox seek: {err}")))?;
    let mut items: Vec<Item> = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(n) = value.get("n").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        if let Some(at) = value.get("delivered").and_then(serde_json::Value::as_str) {
            if let Some(item) = items.iter_mut().find(|item| item.n == n) {
                item.delivered_at = Some(at.to_string());
            }
            continue;
        }
        let Some(kind) = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .and_then(Kind::parse)
        else {
            continue;
        };
        let Some(body) = value.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(at) = value.get("at").and_then(serde_json::Value::as_str) else {
            continue;
        };
        items.push(Item {
            n,
            kind,
            text: body.to_string(),
            at: at.to_string(),
            delivered_at: None,
        });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_delivers_in_order_and_refuses_a_finished_job() {
        let dir = std::env::temp_dir().join(format!("crew-inbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let first = append(&dir, Kind::Queue, "later").unwrap();
        let second = append(&dir, Kind::Steer, "now").unwrap();
        assert_eq!(pending(&dir).unwrap().len(), 2);
        mark_delivered(&dir, first.n).unwrap();
        let left = pending(&dir).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].n, second.n);
        assert_eq!(left[0].kind, Kind::Steer);
        let done = delivered(&dir).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].kind, Kind::Queue);
        assert!(done[0].delivered_at.is_some());
        let _ = fs::remove_dir_all(&dir);

        assert!(require_running(Status::Running).is_ok());
        assert_eq!(require_running(Status::Lost).unwrap_err().code, 3);
        assert_eq!(require_running(Status::Ok).unwrap_err().code, 2);
        assert_eq!(require_running(Status::Stopped).unwrap_err().code, 2);
    }
}
