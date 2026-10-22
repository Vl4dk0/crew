//! Token counts of Antigravity (`agy`) conversations, for the api-dollar line.
//!
//! Each conversation is a SQLite database of protobuf rows. A `steps` row of
//! type 15 is one model response; its metadata holds the times (field 1
//! created, 8 completed) and the usage (field 9: 2 fresh input, 3 output, 5
//! cache read). Output already includes thinking, since field 3 equals the sum
//! of thinking (9) and the rest (10) in every response on record, so thinking
//! is not added again. The model is in `gen_metadata`: field 19 of the request
//! (field 1), answering the step after its `last_step_index` entry (field 20).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RESPONSE_STEP: &str = "15";

pub(super) struct Response {
    pub model: Option<String>,
    pub fresh: u64,
    pub cache_read: u64,
    pub output: u64,
}

pub(super) fn responses(start: SystemTime, end: SystemTime) -> Vec<Response> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root = PathBuf::from(home).join(".gemini/antigravity-cli/conversations");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "db") {
            continue;
        }
        let wal = wal_path(&path);
        let modified = [&path, &wal]
            .iter()
            .filter_map(|file| {
                std::fs::metadata(file)
                    .and_then(|meta| meta.modified())
                    .ok()
            })
            .max();
        if modified.is_none_or(|modified| modified < start) {
            continue;
        }
        found.extend(read_conversation(&path, wal.exists(), start, end));
    }
    found
}

fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// A conversation that is closed has no `-wal`, and a read-only open cannot
/// create the `-shm` it would need, so it is opened as immutable. One still
/// being written is opened normally so its `-wal` is read.
fn read_conversation(path: &Path, live: bool, start: SystemTime, end: SystemTime) -> Vec<Response> {
    let escaped = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let mode = if live { "mode=ro" } else { "immutable=1" };
    let query = format!(
        "SELECT 's', idx, hex(metadata) FROM steps WHERE step_type = {RESPONSE_STEP} AND metadata IS NOT NULL \
         UNION ALL SELECT 'g', idx, hex(data) FROM gen_metadata;"
    );
    let Ok(output) = Command::new("sqlite3")
        .args(["-readonly", "-separator", "|"])
        .arg(format!("file:{escaped}?{mode}"))
        .arg(query)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut steps = Vec::new();
    let mut models = HashMap::new();
    for line in text.lines() {
        let mut cells = line.splitn(3, '|');
        let (Some(kind), Some(idx), Some(blob)) = (cells.next(), cells.next(), cells.next()) else {
            continue;
        };
        let (Ok(idx), Some(blob)) = (idx.parse::<u64>(), unhex(blob)) else {
            continue;
        };
        if kind == "s" {
            steps.push((idx, blob));
        } else if let Some((step, model)) = request_model(&blob) {
            models.insert(step, model);
        }
    }
    steps
        .into_iter()
        .filter_map(|(idx, blob)| {
            let (at, mut response) = step_response(&blob)?;
            if at < start || at >= end {
                return None;
            }
            response.model = models.remove(&idx);
            Some(response)
        })
        .collect()
}

fn step_response(metadata: &[u8]) -> Option<(SystemTime, Response)> {
    let secs = [8, 1].into_iter().find_map(|field| {
        let time = bytes_field(metadata, field)?;
        varint_field(time, 1)
    })?;
    let usage = bytes_field(metadata, 9)?;
    let response = Response {
        model: None,
        fresh: varint_field(usage, 2).unwrap_or(0),
        cache_read: varint_field(usage, 5).unwrap_or(0),
        output: varint_field(usage, 3).unwrap_or(0),
    };
    if response.fresh + response.cache_read + response.output == 0 {
        return None;
    }
    Some((UNIX_EPOCH + Duration::from_secs(secs), response))
}

/// The step a model request answered, and the model that served it.
fn request_model(gen_metadata: &[u8]) -> Option<(u64, String)> {
    let request = bytes_field(gen_metadata, 1)?;
    let model = String::from_utf8(bytes_field(request, 19)?.to_vec()).ok()?;
    let last_step = fields(request)?.into_iter().find_map(|(number, value)| {
        let Field::Bytes(entry) = value else {
            return None;
        };
        if number != 20 || bytes_field(entry, 1)? != b"last_step_index" {
            return None;
        }
        std::str::from_utf8(bytes_field(entry, 2)?)
            .ok()?
            .parse::<u64>()
            .ok()
    })?;
    Some((last_step + 1, model))
}

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// Top-level fields of a protobuf message, without its schema.
fn fields(mut buf: &[u8]) -> Option<Vec<(u64, Field<'_>)>> {
    let mut found = Vec::new();
    while !buf.is_empty() {
        let key = take_varint(&mut buf)?;
        let value = match key & 7 {
            0 => Field::Varint(take_varint(&mut buf)?),
            1 => {
                buf = buf.get(8..)?;
                continue;
            }
            2 => {
                let len = usize::try_from(take_varint(&mut buf)?).ok()?;
                let bytes = buf.get(..len)?;
                buf = &buf[len..];
                Field::Bytes(bytes)
            }
            5 => {
                buf = buf.get(4..)?;
                continue;
            }
            _ => return None,
        };
        found.push((key >> 3, value));
    }
    Some(found)
}

fn take_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for (index, byte) in buf.iter().enumerate() {
        if index >= 10 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            *buf = &buf[index + 1..];
            return Some(value);
        }
    }
    None
}

fn varint_field(buf: &[u8], number: u64) -> Option<u64> {
    fields(buf)?
        .into_iter()
        .find_map(|(field, value)| match value {
            Field::Varint(value) if field == number => Some(value),
            _ => None,
        })
}

fn bytes_field(buf: &[u8], number: u64) -> Option<&[u8]> {
    fields(buf)?
        .into_iter()
        .find_map(|(field, value)| match value {
            Field::Bytes(bytes) if field == number => Some(bytes),
            _ => None,
        })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let low = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(low);
                return out;
            }
            out.push(low | 0x80);
        }
    }

    fn num(field: u64, value: u64) -> Vec<u8> {
        [varint(field << 3), varint(value)].concat()
    }

    fn msg(field: u64, body: &[u8]) -> Vec<u8> {
        [
            varint(field << 3 | 2),
            varint(body.len() as u64),
            body.to_vec(),
        ]
        .concat()
    }

    #[test]
    fn response_step_and_its_model_decode() {
        let usage = [num(2, 14_845), num(3, 1_169), num(5, 30_000), num(9, 1_125)].concat();
        let step = [msg(8, &num(1, 1_792_460_484)), msg(9, &usage)].concat();
        let (at, response) = step_response(&step).expect("response");
        assert_eq!(at, UNIX_EPOCH + Duration::from_secs(1_792_460_484));
        assert_eq!(
            (response.fresh, response.cache_read, response.output),
            (14_845, 30_000, 1_169)
        );

        let entry = [msg(1, b"last_step_index"), msg(2, b"2")].concat();
        let request = [msg(19, b"gemini-3.8-flash-n"), msg(20, &entry)].concat();
        assert_eq!(
            request_model(&msg(1, &request)),
            Some((3, "gemini-3.8-flash-n".to_string()))
        );
    }
}
