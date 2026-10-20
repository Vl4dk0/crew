#![allow(clippy::collapsible_if)]

pub mod models;
pub mod process;
pub mod store;
pub mod supervisor;

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::cli::parse_trailing;
use crate::error::Error;
use crate::id::Provider;
use models::{RunRecord, Status, status_of, validate_run_name};
use process::is_process_alive;
use store::{load_record, run_dir, runs_dir, save_record, tail_lines};

/// A job the supervisor has not started yet.
pub struct NewJob {
    pub name: String,
    pub provider: Provider,
    pub note: Option<String>,
    pub trailing: Vec<String>,
}

fn current_iso() -> String {
    let now = SystemTime::now();
    let duration = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let nanos = duration.subsec_nanos();

    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    let z = days as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mon = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mon <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        year,
        mon,
        d,
        h,
        m,
        s,
        nanos / 1000
    )
}

fn parse_iso_time(s: &str) -> Option<SystemTime> {
    // Basic parser for RFC3339 timestamps e.g. 2026-10-20T20:00:00.000000Z
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<u64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_clean = parts[1].trim_end_matches('Z');
    let time_sub: Vec<&str> = time_clean.split('.').collect();
    let hms: Vec<u64> = time_sub[0]
        .split(':')
        .filter_map(|p| p.parse().ok())
        .collect();

    if date_parts.len() != 3 || hms.len() != 3 {
        return None;
    }

    let y = date_parts[0];
    let m = date_parts[1];
    let d = date_parts[2];

    // Howard Hinnant days from civil date
    let y_adj = if m <= 2 { y - 1 } else { y } as i64;
    let m_adj = if m <= 2 { m + 9 } else { m - 3 };
    let era = (if y_adj >= 0 { y_adj } else { y_adj - 399 }) / 400;
    let yoe = (y_adj - era * 400) as u64;
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    let secs = (days as u64) * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2];
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn format_age(created_at: &str) -> String {
    if let Some(t) = parse_iso_time(created_at) {
        if let Ok(elapsed) = SystemTime::now().duration_since(t) {
            let secs = elapsed.as_secs();
            if secs < 60 {
                return format!("{}s", secs);
            }
            let mins = secs / 60;
            if mins < 60 {
                return format!("{}m", mins);
            }
            let hours = mins / 60;
            if hours < 24 {
                return format!("{}h", hours);
            }
            let days = hours / 24;
            return format!("{}d", days);
        }
    }
    "?".to_string()
}

pub fn run(job: NewJob) -> Result<i32, Error> {
    if !validate_run_name(&job.name) {
        return Err(Error::new(
            2,
            format!(
                "invalid run name {} (must match ^[a-z0-9][a-z0-9._-]{{0,63}}$)",
                job.name
            ),
        ));
    }

    let dir = run_dir(&job.name);
    if dir.exists() {
        return Err(Error::new(2, format!("run {} already exists", job.name)));
    }

    let invocation = parse_trailing(job.provider, &job.trailing)?;

    // Extract prompt bytes
    let prompt_bytes = if let Some(ref p) = invocation.prompt {
        p.as_bytes().to_vec()
    } else {
        // Read from stdin if piped
        let mut buffer = Vec::new();
        let mut stdin = std::io::stdin();
        let _ = stdin.read_to_end(&mut buffer);
        buffer
    };

    if prompt_bytes.is_empty() {
        return Err(Error::new(2, "empty prompt"));
    }

    let cwd = invocation
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .to_string_lossy()
        .to_string();

    fs::create_dir_all(&dir)
        .map_err(|e| Error::new(2, format!("failed to create dir {}: {}", dir.display(), e)))?;

    // Write prompt.bin
    fs::write(dir.join("prompt.bin"), &prompt_bytes)
        .map_err(|e| Error::new(2, format!("failed to write prompt: {}", e)))?;

    let record = RunRecord {
        name: job.name.clone(),
        provider: job.provider.as_str().to_string(),
        args: job.trailing.clone(),
        cwd,
        note: job.note,
        parent: None,
        resume_session: None,
        created_at: current_iso(),
        started_at: None,
        finished_at: None,
        supervisor_pid: None,
        supervisor_start: None,
        wrapper_pid: None,
        session_id: None,
        exit_code: None,
        stopped: false,
        usage: None,
    };

    save_record(&job.name, &record)?;

    // Spawn detached supervisor
    let current_exe = std::env::current_exe()
        .map_err(|e| Error::new(127, format!("failed to find current exe: {}", e)))?;

    Command::new(current_exe)
        .arg("--supervise")
        .arg(&job.name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::new(127, format!("failed to spawn supervisor: {}", e)))?;

    println!("{}", job.name);
    Ok(0)
}

pub fn ls(json: bool) -> Result<i32, Error> {
    let r_dir = runs_dir();
    if !r_dir.is_dir() {
        if json {
            println!("[]");
        } else {
            println!("No runs yet.");
        }
        return Ok(0);
    }

    let mut records = Vec::new();
    if let Ok(entries) = fs::read_dir(&r_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(rec) = load_record(&name) {
                    let alive = is_process_alive(
                        rec.supervisor_pid.unwrap_or(0),
                        rec.supervisor_start.as_deref(),
                    );
                    let status = status_of(&rec, alive);
                    records.push((rec, status));
                }
            }
        }
    }

    if records.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No runs yet.");
        }
        return Ok(0);
    }

    // Sort chronologically ascending
    records.sort_by(|a, b| a.0.created_at.cmp(&b.0.created_at));

    if json {
        let mut json_arr = Vec::new();
        for (rec, status) in records {
            let mut val = serde_json::to_value(&rec).unwrap();
            val["status"] = serde_json::Value::String(status.as_str().to_string());
            json_arr.push(val);
        }
        println!("{}", serde_json::to_string_pretty(&json_arr).unwrap());
    } else {
        println!(
            "{:<14}  {:<8}  {:<10}  {:<23}  {:<5}",
            "NAME", "STATUS", "WRAPPER", "MODEL", "AGE"
        );
        for (rec, status) in records {
            let model = rec
                .args
                .windows(2)
                .find(|w| w[0] == "-m" || w[0] == "--model")
                .map(|w| w[1].as_str())
                .unwrap_or("default");

            let age = format_age(&rec.created_at);
            println!(
                "{:<14}  {:<8}  {:<10}  {:<23}  {:<5}",
                rec.name,
                status.as_str(),
                rec.provider,
                model,
                age
            );
        }
    }

    Ok(0)
}

pub fn show(name: &str, json: bool) -> Result<i32, Error> {
    let rec = load_record(name)?;
    let alive = is_process_alive(
        rec.supervisor_pid.unwrap_or(0),
        rec.supervisor_start.as_deref(),
    );
    let status = status_of(&rec, alive);

    if json {
        let mut val = serde_json::to_value(&rec).unwrap();
        val["status"] = serde_json::Value::String(status.as_str().to_string());
        println!("{}", serde_json::to_string_pretty(&val).unwrap());
    } else {
        println!("name: {}", rec.name);
        println!("provider: {}", rec.provider);
        println!("args: {:?}", rec.args);
        println!("cwd: {}", rec.cwd);
        if let Some(ref note) = rec.note {
            println!("note: {}", note);
        }
        if let Some(ref parent) = rec.parent {
            println!("parent: {}", parent);
        }
        println!("created_at: {}", rec.created_at);
        if let Some(ref s) = rec.started_at {
            println!("started_at: {}", s);
        }
        if let Some(ref f) = rec.finished_at {
            println!("finished_at: {}", f);
        }
        if let Some(pid) = rec.supervisor_pid {
            println!("supervisor_pid: {}", pid);
        }
        if let Some(ref start) = rec.supervisor_start {
            println!("supervisor_start: {}", start);
        }
        if let Some(pid) = rec.wrapper_pid {
            println!("wrapper_pid: {}", pid);
        }
        if let Some(ref sid) = rec.session_id {
            println!("session_id: {}", sid);
        }
        if let Some(code) = rec.exit_code {
            println!("exit_code: {}", code);
        }
        println!("stopped: {}", rec.stopped);
        println!("status: {}", status.as_str());
    }

    Ok(0)
}

fn extract_model(rec: &RunRecord) -> &str {
    rec.args
        .windows(2)
        .find(|w| w[0] == "-m" || w[0] == "--model")
        .map(|w| w[1].as_str())
        .unwrap_or(&rec.provider)
}

fn run_duration(rec: &RunRecord) -> Option<Duration> {
    let s = parse_iso_time(rec.started_at.as_deref()?)?;
    let f = parse_iso_time(rec.finished_at.as_deref()?)?;
    f.duration_since(s).ok()
}

pub fn result(name: &str) -> Result<i32, Error> {
    let rec = load_record(name)?;
    let alive = is_process_alive(
        rec.supervisor_pid.unwrap_or(0),
        rec.supervisor_start.as_deref(),
    );
    let status = status_of(&rec, alive);

    if status == Status::Running {
        eprintln!("crew: {} is running; stdout is incomplete", name);
        return Err(Error::new(2, "running"));
    }
    if status == Status::Lost {
        eprintln!("crew: {} is lost; stdout is unavailable", name);
        return Err(Error::new(3, "lost"));
    }

    let stdout_path = run_dir(name).join("stdout");
    if !stdout_path.is_file() {
        return Err(Error::new(2, "stdout is missing"));
    }

    let bytes = fs::read(&stdout_path)
        .map_err(|e| Error::new(2, format!("failed to read stdout: {}", e)))?;
    std::io::stdout()
        .write_all(&bytes)
        .map_err(|e| Error::new(2, format!("write error: {}", e)))?;

    if let Some(dur) = run_duration(&rec) {
        if let Some(note) = crate::review::long_run_note(dur, extract_model(&rec)) {
            eprintln!("{}", note);
        }
    }

    Ok(0)
}

pub fn log(name: &str, lines: usize) -> Result<i32, Error> {
    let rec = load_record(name)?;
    let alive = is_process_alive(
        rec.supervisor_pid.unwrap_or(0),
        rec.supervisor_start.as_deref(),
    );
    let status = status_of(&rec, alive);

    let stderr_path = run_dir(name).join("stderr");
    if !stderr_path.is_file() {
        if status == Status::Running {
            return Err(Error::new(
                2,
                format!("{} is running and stderr missing", name),
            ));
        }
        if status == Status::Lost {
            return Err(Error::new(
                3,
                format!("{} is lost and stderr missing", name),
            ));
        }
    }

    let output = tail_lines(&stderr_path, lines)?;
    print!("{}", output);
    Ok(0)
}

pub fn wait(names: &[String], any: bool, timeout_secs: Option<u64>) -> Result<i32, Error> {
    if names.is_empty() {
        return Ok(0);
    }

    let start = Instant::now();
    let timeout = timeout_secs.map(Duration::from_secs);

    let mut pending: Vec<String> = names.to_vec();
    let mut done: Vec<(String, i32)> = Vec::new();

    loop {
        let mut i = 0;
        while i < pending.len() {
            let name = &pending[i];
            if let Ok(rec) = load_record(name) {
                let alive = is_process_alive(
                    rec.supervisor_pid.unwrap_or(0),
                    rec.supervisor_start.as_deref(),
                );
                let status = status_of(&rec, alive);
                if status != Status::Running {
                    let code = if status == Status::Stopped {
                        130
                    } else {
                        rec.exit_code.unwrap_or(0)
                    };

                    if let Some(dur) = run_duration(&rec) {
                        if let Some(note) = crate::review::long_run_note(dur, extract_model(&rec)) {
                            eprintln!("{}", note);
                        }
                    }

                    if any {
                        println!("{}", name);
                        return Ok(code);
                    }

                    done.push((name.clone(), code));
                    pending.remove(i);
                    continue;
                }
            }
            i += 1;
        }

        if pending.is_empty() {
            // Find first non-zero in original argument order
            for name in names {
                if let Some((_, code)) = done.iter().find(|(n, _)| n == name) {
                    if *code != 0 {
                        return Ok(*code);
                    }
                }
            }
            return Ok(0);
        }

        if let Some(limit) = timeout {
            if start.elapsed() >= limit {
                eprintln!("timed out waiting");
                return Err(Error::new(4, "timed out waiting"));
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn resume(name: &str, prompt: Option<String>) -> Result<i32, Error> {
    let parent = load_record(name)?;
    let session_id = parent
        .session_id
        .ok_or_else(|| Error::new(2, format!("run {} has no session id to resume", name)))?;

    // Determine candidate name: <base>.<N>
    let base_name = if let Some(idx) = name.rfind('.') {
        if name[idx + 1..].chars().all(|c| c.is_ascii_digit()) {
            &name[..idx]
        } else {
            name
        }
    } else {
        name
    };

    let mut n = 2;
    let mut next_name = format!("{}.{}", base_name, n);
    while run_dir(&next_name).exists() {
        n += 1;
        next_name = format!("{}.{}", base_name, n);
    }

    let provider: Provider = parent.provider.parse()?;

    // Build new args reusing parent args, adding -r <session_id>
    let mut new_args = Vec::new();
    let mut skip_next = false;
    for (i, arg) in parent.args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-r" || arg == "--resume" {
            skip_next = true;
            continue;
        }
        if arg == "-c" || arg == "--continue" {
            continue;
        }
        // Omit trailing prompt from parent args
        if i == parent.args.len() - 1 && !arg.starts_with('-') {
            continue;
        }
        new_args.push(arg.clone());
    }

    new_args.push("-r".to_string());
    new_args.push(session_id.clone());

    if let Some(p) = prompt {
        new_args.push(p);
    }

    let new_job = NewJob {
        name: next_name.clone(),
        provider,
        note: parent.note.clone(),
        trailing: new_args,
    };

    run(new_job)
}

pub fn stop(name: &str) -> Result<i32, Error> {
    let rec = load_record(name)?;
    if let Some(pid) = rec.supervisor_pid {
        if is_process_alive(pid, rec.supervisor_start.as_deref()) {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    Ok(0)
}

pub fn supervise(name: &str) -> Result<i32, Error> {
    supervisor::run_supervisor(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_formatting() {
        let now_iso = current_iso();
        assert!(format_age(&now_iso).ends_with('s'));

        // Bad iso returns '?'
        assert_eq!(format_age("not-iso"), "?");
    }

    #[test]
    fn resume_next_name_logic() {
        let base = "task";
        assert_eq!(format!("{}.2", base), "task.2");

        let dotted = "task.2";
        let base_extracted = if let Some(idx) = dotted.rfind('.') {
            if dotted[idx + 1..].chars().all(|c| c.is_ascii_digit()) {
                &dotted[..idx]
            } else {
                dotted
            }
        } else {
            dotted
        };
        assert_eq!(base_extracted, "task");
    }
}
