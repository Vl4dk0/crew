use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::job::models::RunRecord;

pub fn base_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CREW_HOME") {
        PathBuf::from(dir)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/crew")
    } else {
        PathBuf::from(".crew")
    }
}

pub fn runs_dir() -> PathBuf {
    base_dir().join("runs")
}

pub fn run_dir(name: &str) -> PathBuf {
    runs_dir().join(name)
}

/// Open an existing file safely (O_NOFOLLOW | O_NONBLOCK) and verify it is a regular file.
pub fn open_safe_read(path: &Path) -> Result<File, Error> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| Error::new(2, format!("failed to open {}: {}", path.display(), e)))?;

    // Check with fstat
    use std::os::unix::io::AsRawFd;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::fstat(file.as_raw_fd(), &mut stat) };
    if res != 0 {
        return Err(Error::new(2, format!("fstat failed on {}", path.display())));
    }

    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(Error::new(
            2,
            format!("not a regular file: {}", path.display()),
        ));
    }

    Ok(file)
}

/// Read and deserialize `run.json` from `<run_dir>/run.json`.
pub fn load_record(name: &str) -> Result<RunRecord, Error> {
    let path = run_dir(name).join("run.json");
    let mut file = open_safe_read(&path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| Error::new(2, format!("failed to read {}: {}", path.display(), e)))?;

    serde_json::from_str(&content)
        .map_err(|e| Error::new(2, format!("corrupt run.json for {}: {}", name, e)))
}

/// Save `run.json` atomically via write + rename.
pub fn save_record(name: &str, record: &RunRecord) -> Result<(), Error> {
    let dir = run_dir(name);
    fs::create_dir_all(&dir)
        .map_err(|e| Error::new(2, format!("failed to create dir {}: {}", dir.display(), e)))?;

    let tmp_path = dir.join("run.json.tmp");
    let final_path = dir.join("run.json");

    let json_bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| Error::new(2, format!("serialize error: {}", e)))?;

    fs::write(&tmp_path, json_bytes)
        .map_err(|e| Error::new(2, format!("failed to write tmp run.json: {}", e)))?;

    fs::rename(&tmp_path, &final_path)
        .map_err(|e| Error::new(2, format!("failed to commit run.json: {}", e)))?;

    Ok(())
}

/// Tail the last `lines` of a file.
pub fn tail_lines(path: &Path, count: usize) -> Result<String, Error> {
    if !path.is_file() {
        return Ok(String::new());
    }
    let mut file = open_safe_read(path)?;
    let file_len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| Error::new(2, format!("seek error: {}", e)))?;

    if file_len == 0 || count == 0 {
        return Ok(String::new());
    }

    const CHUNK_SIZE: u64 = 8192;
    let mut pos = file_len;
    let mut found_lines = 0;
    let mut buffer = Vec::new();

    while pos > 0 && found_lines <= count {
        let read_len = pos.min(CHUNK_SIZE);
        pos -= read_len;
        file.seek(SeekFrom::Start(pos))
            .map_err(|e| Error::new(2, format!("seek error: {}", e)))?;

        let mut chunk = vec![0u8; read_len as usize];
        file.read_exact(&mut chunk)
            .map_err(|e| Error::new(2, format!("read error: {}", e)))?;

        for &b in chunk.iter().rev() {
            if b == b'\n' {
                found_lines += 1;
                if found_lines > count {
                    break;
                }
            }
        }
        chunk.extend(buffer);
        buffer = chunk;
    }

    let text = String::from_utf8_lossy(&buffer);
    let all_lines: Vec<&str> = text.lines().collect();
    let start_idx = all_lines.len().saturating_sub(count);
    let selected = &all_lines[start_idx..];
    let mut result = selected.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}
