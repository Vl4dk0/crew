use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Bound on reviews kept in the log.
pub const MAX_REVIEWS: usize = 30;

/// Durable record of one subagent run rating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub when: String,
    pub model: String,
    pub task: String,
    pub what_they_did: String,
    pub duration_s: u64,
    pub cost: Option<f64>,
    pub score: u8,
    pub reviewer: String,
}

/// Format current UTC timestamp in ISO-8601 / RFC-3339 representation.
pub fn now_utc_rfc3339() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let micros = duration.subsec_micros();

    let days = (secs / 86400) as i64;
    let rem_secs = (secs % 86400) as u32;
    let hour = rem_secs / 3600;
    let minute = (rem_secs % 3600) / 60;
    let second = rem_secs % 60;

    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142401) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z")
}

/// Standard location for reviewsubagent history.
pub fn reviews_path() -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::new(2, "HOME environment variable not set"))?;
    Ok(Path::new(&home).join(".local/share/reviewsubagent/reviews.jsonl"))
}

/// Load existing reviews from the specified file. Missing file yields an empty vector.
pub fn load_from(path: &Path) -> Result<Vec<Review>, Error> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::new(2, format!("failed to read reviews file: {err}"))),
    };

    let mut reviews = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let review: Review = serde_json::from_str(line).map_err(|err| {
            Error::new(
                2,
                format!(
                    "invalid review at {}:{}: {err}",
                    path.display(),
                    line_no + 1
                ),
            )
        })?;
        reviews.push(review);
    }
    Ok(reviews)
}

/// Load reviews from the standard store location.
pub fn load() -> Result<Vec<Review>, Error> {
    let path = reviews_path()?;
    load_from(&path)
}

/// Append a review to the specified path, capping store length at 30 with atomic replacement.
pub fn append_to(path: &Path, review: Review) -> Result<(), Error> {
    let mut reviews = load_from(path)?;
    reviews.push(review);
    if reviews.len() > MAX_REVIEWS {
        let start = reviews.len() - MAX_REVIEWS;
        reviews = reviews.split_off(start);
    }

    let parent = path
        .parent()
        .ok_or_else(|| Error::new(2, format!("invalid reviews path: {}", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|err| Error::new(2, format!("failed to create review directory: {err}")))?;

    let temp_name = format!(
        "reviews.jsonl.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let temp_path = parent.join(temp_name);

    let write_result = (|| -> Result<(), Error> {
        use std::io::Write;
        let mut file = fs::File::create(&temp_path).map_err(|err| {
            Error::new(2, format!("failed to create temporary review file: {err}"))
        })?;
        for item in &reviews {
            let json_line = serde_json::to_string(item)
                .map_err(|err| Error::new(2, format!("failed to serialize review: {err}")))?;
            writeln!(file, "{json_line}")
                .map_err(|err| Error::new(2, format!("failed to write review record: {err}")))?;
        }
        file.sync_all()
            .map_err(|err| Error::new(2, format!("failed to sync temporary review file: {err}")))?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::new(
            2,
            format!("failed to replace review file atomically: {err}"),
        ));
    }

    Ok(())
}

/// Append a review to the standard store.
pub fn append(review: Review) -> Result<(), Error> {
    let path = reviews_path()?;
    append_to(&path, review)
}
