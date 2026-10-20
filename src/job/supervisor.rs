use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::error::Error;
use crate::job::process::get_process_lstart;
use crate::job::store::{load_record, run_dir, save_record};

fn current_iso_time() -> String {
    let now = SystemTime::now();
    chrono_free_iso(now)
}

fn chrono_free_iso(now: SystemTime) -> String {
    // Generate ISO 8601 UTC string using system time
    let duration = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let nanos = duration.subsec_nanos();

    // Convert epoch seconds to date-time
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    // Civil date algorithm (Howard Hinnant)
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

/// Daemon supervisor that executes `crew <provider> <args...>` and monitors execution.
pub fn run_supervisor(name: &str) -> Result<i32, Error> {
    // Create new session
    unsafe {
        libc::setsid();
    }

    let my_pid = std::process::id();
    let my_start = get_process_lstart(my_pid).unwrap_or_default();

    let mut record = load_record(name)?;
    record.supervisor_pid = Some(my_pid);
    record.supervisor_start = Some(my_start);
    save_record(name, &record)?;

    let dir = run_dir(name);
    let prompt_file = dir.join("prompt.bin");
    let prompt_bytes = fs::read(&prompt_file).unwrap_or_default();

    // Prepare child process
    let current_exe = std::env::current_exe()
        .map_err(|e| Error::new(127, format!("failed to find current exe: {}", e)))?;

    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");

    let stdout_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&stdout_path)
        .map_err(|e| Error::new(127, format!("failed to open stdout: {}", e)))?;

    let stderr_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&stderr_path)
        .map_err(|e| Error::new(127, format!("failed to open stderr: {}", e)))?;

    let mut child_cmd = Command::new(&current_exe);
    child_cmd.arg(&record.provider);

    // Pass args, ensuring --print-id is included
    let mut has_print_id = false;
    for arg in &record.args {
        if arg == "--print-id" {
            has_print_id = true;
        }
        child_cmd.arg(arg);
    }
    if !has_print_id {
        child_cmd.arg("--print-id");
    }

    // Pass `-` so prompt is read from stdin
    child_cmd.arg("-");

    if !record.cwd.is_empty() {
        child_cmd.current_dir(Path::new(&record.cwd));
    }

    child_cmd.stdin(Stdio::piped());
    child_cmd.stdout(Stdio::from(stdout_file));
    child_cmd.stderr(Stdio::from(stderr_file));

    // Put wrapper in its own process group
    child_cmd.process_group(0);

    let start_instant = Instant::now();
    record.started_at = Some(current_iso_time());

    let mut child = match child_cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            record.exit_code = Some(127);
            record.finished_at = Some(current_iso_time());
            let _ = save_record(name, &record);
            return Err(Error::new(127, format!("failed to spawn wrapper: {}", e)));
        }
    };

    let wrapper_pid = child.id();
    record.wrapper_pid = Some(wrapper_pid);
    save_record(name, &record)?;

    // Deliver prompt
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&prompt_bytes);
        let _ = stdin.flush();
    }

    // Set up signal handler flag for SIGTERM
    static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
    unsafe {
        libc::signal(libc::SIGTERM, handle_sigterm as *const () as libc::size_t);
    }
    extern "C" fn handle_sigterm(_sig: libc::c_int) {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
    }

    let mut exit_code = None;

    // Poll loop waiting for child or SIGTERM
    loop {
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            record.stopped = true;
            // Send SIGTERM to wrapper process group
            unsafe {
                libc::killpg(wrapper_pid as i32, libc::SIGTERM);
            }
            let kill_deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < kill_deadline {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status
                            .code()
                            .unwrap_or_else(|| 128 + status.signal().unwrap_or(15));
                        exit_code = Some(code);
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
            if exit_code.is_none() {
                // Hard kill after 10s grace
                unsafe {
                    libc::killpg(wrapper_pid as i32, libc::SIGKILL);
                }
                let _ = child.wait();
                exit_code = Some(137); // 128 + 9
            }
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
                exit_code = Some(code);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_e) => {
                exit_code = Some(127);
                break;
            }
        }
    }

    let elapsed = start_instant.elapsed();
    record.finished_at = Some(current_iso_time());
    record.exit_code = exit_code;

    // Scan stderr for SESSION_ID=<id>
    if let Ok(stderr_content) = fs::read_to_string(dir.join("stderr")) {
        for line in stderr_content.lines() {
            if let Some(pos) = line.find("SESSION_ID=") {
                let id = line[pos + 11..].trim().to_string();
                if !id.is_empty() {
                    record.session_id = Some(id);
                }
            }
        }

        // Check if run took > 20 minutes (1200 seconds)
        if elapsed >= Duration::from_secs(1200) {
            let mins = elapsed.as_secs() / 60;
            let secs = elapsed.as_secs() % 60;
            let reminder = format!(
                "\n[crew] Note: Agent run took {}m{:02}s (> 20m). Consider rating it with: crew review write <model> \"<task>\"\n",
                mins, secs
            );
            if let Ok(mut sf) = fs::OpenOptions::new().append(true).open(dir.join("stderr")) {
                let _ = sf.write_all(reminder.as_bytes());
            }
        }
    }

    save_record(name, &record)?;
    Ok(exit_code.unwrap_or(0))
}
