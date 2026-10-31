use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::id::Provider;
use crate::job::inbox::{self, Kind};
use crate::job::process::get_process_lstart;
use crate::job::store::{load_record, run_dir, save_record};

/// Claude, pi, and Codex speak steer on the child's stdin. Grok, Gemini, and
/// a Codex review do not, so the supervisor cuts that step and resumes.
pub fn native_steer(provider: &str, args: &[String]) -> bool {
    match provider.parse::<Provider>() {
        Ok(Provider::Claude | Provider::Pi) => true,
        Ok(Provider::Codex) => !args.iter().any(|arg| arg == "--review"),
        Ok(Provider::Grok | Provider::Gemini) => false,
        Err(_) => crate::tier::agent(provider).ok().flatten().is_some(),
    }
}

enum Turn {
    /// Child exited 0. Pending inbox lines become the next turn.
    Ok,
    /// Child failed on its own. Leave the inbox for a later look.
    Failed(i32),
    Stopped(i32),
    /// Fallback steer killed the child. `text` is already wrapped.
    Fallback(String),
}

/// Daemon supervisor that executes `crew <provider> <args...>` and, when the
/// turn ends cleanly with mail still in the inbox, runs the next message in
/// the same job and the same session.
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
    let mut prompt = fs::read(dir.join("prompt.bin")).unwrap_or_default();
    let mut resume_id: Option<String> = None;

    static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
    unsafe {
        libc::signal(libc::SIGTERM, handle_sigterm as *const () as libc::size_t);
    }
    extern "C" fn handle_sigterm(_sig: libc::c_int) {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
    }

    let exit_code = loop {
        if prompt.is_empty() && resume_id.is_some() {
            break 0;
        }
        match run_turn(
            name,
            &mut record,
            &dir,
            &prompt,
            resume_id.as_deref(),
            &STOP_REQUESTED,
        )? {
            Turn::Stopped(code) => break code,
            Turn::Failed(code) => break code,
            Turn::Fallback(text) => {
                let Some(session) = record.session_id.clone() else {
                    note_line(&dir, "[crew] steer left pending: no session id");
                    break 0;
                };
                prompt = text.into_bytes();
                resume_id = Some(session);
            }
            Turn::Ok => {
                let pending = inbox::pending(&dir)?;
                let Some(next) = pending.into_iter().next() else {
                    break 0;
                };
                let Some(session) = record.session_id.clone() else {
                    note_line(&dir, "[crew] inbox message left pending: no session id");
                    break 0;
                };
                inbox::mark_delivered(&dir, next.n)?;
                prompt = next.text.into_bytes();
                resume_id = Some(session);
            }
        }
    };

    record.finished_at = Some(super::current_iso());
    record.exit_code = Some(exit_code);
    note_session(name, &mut record, &dir);
    save_record(name, &record)?;
    Ok(exit_code)
}

fn run_turn(
    name: &str,
    record: &mut crate::job::models::RunRecord,
    dir: &Path,
    prompt: &[u8],
    resume_id: Option<&str>,
    stop_requested: &AtomicBool,
) -> Result<Turn, Error> {
    let current_exe = std::env::current_exe()
        .map_err(|err| Error::new(127, format!("failed to find current exe: {err}")))?;

    let stdout_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("stdout"))
        .map_err(|err| Error::new(127, format!("failed to open stdout: {err}")))?;
    let stderr_file = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("stderr"))
        .map_err(|err| Error::new(127, format!("failed to open stderr: {err}")))?;

    let mut child_cmd = Command::new(&current_exe);
    child_cmd.arg(&record.provider);
    child_cmd.args(turn_args(&record.args, resume_id));
    if !prompt.is_empty() {
        child_cmd.arg("-");
    }
    if !record.cwd.is_empty() {
        child_cmd.current_dir(Path::new(&record.cwd));
    }
    // The wrapper polls the inbox. The provider CLI must not see this, or a
    // tool it runs would think it was the job.
    child_cmd.env("CREW_JOB", name);
    if prompt.is_empty() {
        child_cmd.stdin(Stdio::null());
    } else {
        child_cmd.stdin(Stdio::piped());
    }
    child_cmd.stdout(Stdio::from(stdout_file));
    child_cmd.stderr(Stdio::from(stderr_file));
    child_cmd.process_group(0);

    if record.started_at.is_none() {
        record.started_at = Some(super::current_iso());
    }

    let mut child = match child_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            record.exit_code = Some(127);
            record.finished_at = Some(super::current_iso());
            let _ = save_record(name, record);
            return Err(Error::new(127, format!("failed to spawn wrapper: {err}")));
        }
    };

    let wrapper_pid = child.id();
    record.wrapper_pid = Some(wrapper_pid);
    // A previous turn's exit must not make this one look finished.
    record.exit_code = None;
    record.finished_at = None;
    record.stopped = false;
    save_record(name, record)?;

    if !prompt.is_empty()
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(prompt);
        let _ = stdin.flush();
    }

    let native = native_steer(&record.provider, &record.args);
    let mut exit_code = None;
    let mut fallback: Option<String> = None;

    loop {
        if stop_requested.load(Ordering::SeqCst) {
            record.stopped = true;
            exit_code = Some(stop_child(&mut child, wrapper_pid));
            break;
        }

        note_session(name, record, dir);

        // A native wrapper injects steer itself. Here a steer means the CLI
        // has no mid-turn channel, so the step is discarded and resumed.
        if !native
            && record.session_id.is_some()
            && let Some(text) = take_fallback_steers(dir)?
        {
            let _ = stop_child(&mut child, wrapper_pid);
            fallback = Some(text);
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                );
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => {
                exit_code = Some(127);
                break;
            }
        }
    }

    note_session(name, record, dir);
    if let Some(text) = fallback {
        return Ok(Turn::Fallback(text));
    }
    let code = exit_code.unwrap_or(0);
    if record.stopped {
        Ok(Turn::Stopped(code))
    } else if code == 0 {
        Ok(Turn::Ok)
    } else {
        Ok(Turn::Failed(code))
    }
}

/// Drop `-r` and `-c` from a later turn so the recorded session is the one
/// we just watched, not the flag the user typed for the first turn.
fn turn_args(args: &[String], resume_id: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if resume_id.is_some() {
            match arg.as_str() {
                "-r" | "--resume" => {
                    skip = true;
                    continue;
                }
                "-c" | "--continue" => continue,
                _ if arg.starts_with("--resume=") => continue,
                _ => {}
            }
        }
        out.push(arg.clone());
    }
    if let Some(id) = resume_id {
        out.push("-r".to_string());
        out.push(id.to_string());
    }
    out
}

fn take_fallback_steers(dir: &Path) -> Result<Option<String>, Error> {
    let pending = inbox::pending(dir)?;
    let steers: Vec<_> = pending
        .into_iter()
        .filter(|item| item.kind == Kind::Steer)
        .collect();
    if steers.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    for (index, item) in steers.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(&item.text);
        inbox::mark_delivered(dir, item.n)?;
    }
    Ok(Some(format!(
        "You were interrupted mid-step by your orchestrator. New instruction: {text}"
    )))
}

fn stop_child(child: &mut std::process::Child, wrapper_pid: u32) -> i32 {
    unsafe {
        libc::killpg(wrapper_pid as i32, libc::SIGTERM);
    }
    let kill_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < kill_deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(15));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    unsafe {
        libc::killpg(wrapper_pid as i32, libc::SIGKILL);
    }
    let _ = child.wait();
    137
}

fn note_line(dir: &Path, line: &str) {
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stderr"))
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

/// Persist the session id as soon as the wrapper prints it. A supervisor
/// that dies after that still leaves a resumable record.
fn note_session(name: &str, record: &mut crate::job::models::RunRecord, dir: &Path) {
    let Ok(stderr_content) = fs::read_to_string(dir.join("stderr")) else {
        return;
    };
    let mut found = None;
    for line in stderr_content.lines() {
        if let Some(pos) = line.find("SESSION_ID=") {
            let id = line[pos + "SESSION_ID=".len()..].trim();
            if !id.is_empty() {
                found = Some(id.to_string());
            }
        }
    }
    let Some(id) = found else {
        return;
    };
    if record.session_id.as_deref() == Some(id.as_str()) {
        return;
    }
    record.session_id = Some(id);
    let _ = save_record(name, record);
}
