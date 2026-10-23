//! Spawn a provider child and turn its status into a crew exit code.
//!
//! The child inherits the environment. Stdin is `/dev/null` except the gemini
//! PTY, and the prompt is never written there. A deadline kills the process
//! group: SIGTERM, five seconds, SIGKILL, except codex, which SIGKILLs at once.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::cli::Invocation;
use crate::error::Error;
use crate::id::Provider;

use super::pty;

pub(super) const LAUNCH_EXIT: i32 = 127;

static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Copy)]
pub(super) enum StdioKind {
    Piped,
    Pty,
}

#[derive(Clone, Copy)]
pub(super) enum KillStyle {
    /// Claude, grok, and gemini. The group gets SIGTERM, then SIGKILL.
    TermThenKill,
    /// Codex's wrapper has no grace period.
    ImmediateKill,
}

pub(super) struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub(super) struct Report {
    pub code: i32,
    pub before_stderr: String,
    pub stdout: String,
    pub after_stderr: String,
}

/// User-facing `max` becomes the wire value each CLI actually accepts.
/// The note is present only when that wire value is not `max`.
pub(super) fn map_effort(
    provider: Provider,
    model: Option<&str>,
    effort: &str,
) -> (String, Option<String>) {
    if effort != "max" {
        return (effort.to_string(), None);
    }
    let wire = match (provider, model) {
        (Provider::Claude, _) => "max",
        (Provider::Codex, Some("gpt-6-luna")) => "max",
        (Provider::Codex, _) => "ultra",
        (Provider::Gemini, _) => "high",
        (Provider::Grok, Some("grok-4.5")) => "high",
        (Provider::Grok, _) => "xhigh",
    };
    let note = (wire != "max").then(|| format!("[crew] effort max -> {wire}"));
    (wire.to_string(), note)
}

/// Only an omitted prompt or a literal `-` consumes crew's stdin.
pub(super) fn reads_process_stdin(prompt: Option<&str>) -> bool {
    matches!(prompt, None | Some("-"))
}

pub(super) fn codex_scope(invocation: &Invocation) -> bool {
    invocation.uncommitted || invocation.base.is_some() || invocation.commit.is_some()
}

pub(super) fn take_prompt(invocation: &Invocation) -> Result<Option<String>, Error> {
    // A codex review scope is the prompt. Reading stdin there would swallow
    // whatever the caller had piped for an unrelated reason.
    if invocation.provider == Provider::Codex && codex_scope(invocation) {
        return Ok(None);
    }
    match invocation.prompt.as_deref() {
        Some(text) if !reads_process_stdin(Some(text)) => {
            if text.trim().is_empty() {
                return Err(Error::new(
                    2,
                    "empty prompt (pass a prompt argument or pipe one via stdin)",
                ));
            }
            Ok(Some(text.to_string()))
        }
        _ => read_stdin_prompt().map(Some),
    }
}

fn read_stdin_prompt() -> Result<String, Error> {
    // SAFETY: isatty only inspects the stdin file descriptor.
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    if tty {
        return Err(Error::new(2, "pass a prompt or pipe one using -"));
    }
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|err| Error::new(2, format!("cannot read prompt from stdin: {err}")))?;
    if buf.trim().is_empty() {
        return Err(Error::new(
            2,
            "empty prompt (pass a prompt argument or pipe one via stdin)",
        ));
    }
    Ok(buf)
}

/// `{...}` is the schema text. Anything else is a tilde-expanded file, matching
/// claude-run and grok-run. Codex does not use this.
pub(super) fn load_schema_literal(schema: &str) -> Result<String, Error> {
    if schema.trim_start().starts_with('{') {
        return Ok(schema.to_string());
    }
    let path = expand_user(schema);
    fs::read_to_string(&path).map_err(|err| Error::new(2, format!("--schema: {err}")))
}

pub(super) fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `Path.resolve()` without tilde expansion. Existing symlink prefixes are
/// resolved; a missing tail is appended as written.
pub(super) fn resolve_no_tilde(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let normalized = lexical_normalize(&absolute);
    let mut resolved = PathBuf::new();
    for component in normalized.components() {
        resolved.push(component);
        if let Ok(canonical) = fs::canonicalize(&resolved) {
            resolved = canonical;
        }
    }
    resolved
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(".."),
                Some(Component::RootDir | Component::Prefix(_) | Component::CurDir) => {}
            },
            other => out.push(other),
        }
    }
    out
}

pub(super) fn timeout_duration(secs: f64) -> Result<Duration, Error> {
    if !secs.is_finite() || secs <= 0.0 {
        return Err(Error::new(2, "timeout must be a positive finite number"));
    }
    Duration::try_from_secs_f64(secs)
        .map_err(|_| Error::new(2, "timeout must be a positive finite number"))
}

pub(super) fn format_secs(secs: f64) -> String {
    if secs.is_finite() && secs.fract() == 0.0 && secs.abs() < 1e16 {
        return format!("{}", secs.trunc() as i64);
    }
    format!("{secs}")
}

pub(super) fn py_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number_truthy(number),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn number_truthy(number: &serde_json::Number) -> bool {
    if let Some(value) = number.as_i64() {
        return value != 0;
    }
    if let Some(value) = number.as_u64() {
        return value != 0;
    }
    number.as_f64().is_some_and(|value| value != 0.0)
}

pub(super) fn py_display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Exit code for a run that stopped because the provider's usage, quota,
/// rate, or credit limit was hit. EX_TEMPFAIL: retrying later can succeed,
/// which is what separates it from a failed or denied run (125).
pub(super) const USAGE_LIMIT_EXIT: i32 = 75;

/// Last-resort text match for CLIs whose limit message carries no structured
/// marker. Callers only ask it about a run that already failed.
pub(super) fn looks_like_usage_limit(text: &str) -> bool {
    let text = text.to_lowercase().replace('\u{2019}', "'");
    [
        "hit your",
        "usage limit",
        "out of usage credits",
        "out of credits",
        "credit balance",
        "rate limit",
        "rate-limit",
        "quota",
        "resource_exhausted",
    ]
    .iter()
    .any(|needle| text.contains(needle))
        && !text.trim().is_empty()
}

/// The one stderr line every provider prints for a usage-limit stop.
pub(super) fn usage_limit_line(provider: &str, message: &str) -> String {
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("[crew] {provider} usage limit: {message}\n")
}

pub(super) fn code_or_one(code: i32) -> i32 {
    if code == 0 { 1 } else { code }
}

/// Python's negative wait status is a signal. Crew exits `128+N`.
pub(super) fn public_code(status: i32) -> i32 {
    if status < 0 {
        128i32.saturating_sub(status)
    } else {
        status
    }
}

pub(super) fn push_pair(argv: &mut Vec<String>, flag: &str, value: &str) {
    argv.push(flag.to_string());
    argv.push(value.to_string());
}

pub(super) fn write_stderr_line(text: &str) {
    if text.ends_with('\n') {
        write_raw_stderr(text);
    } else {
        write_raw_stderr(&format!("{text}\n"));
    }
}

pub(super) fn write_effort_note(note: &Option<String>) {
    if let Some(note) = note {
        write_stderr_line(note);
    }
}

pub(super) fn emit_report(report: &Report) {
    write_raw_stderr(&report.before_stderr);
    write_raw_stdout(&report.stdout);
    write_raw_stderr(&report.after_stderr);
}

fn write_raw_stdout(text: &str) {
    if text.is_empty() {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn write_raw_stderr(text: &str) {
    if text.is_empty() {
        return;
    }
    let mut err = io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

pub(super) fn capture(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    kind: StdioKind,
    kill: KillStyle,
    catch_signals: bool,
) -> io::Result<Captured> {
    let _lock = CAPTURE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    INTERRUPTED.store(0, Ordering::SeqCst);

    let (mut child, readers) = spawn(argv, cwd, kind)?;
    let pid = child.id();
    let _guard = if catch_signals {
        InterruptGuard::install(pid).ok()
    } else {
        None
    };

    let mut timed_out = false;
    let status = if let Some(status) = reap_with_timeout(&mut child, timeout)? {
        status
    } else {
        timed_out = true;
        match kill {
            KillStyle::ImmediateKill => {
                kill_group(pid, libc::SIGKILL);
                child.wait()?
            }
            KillStyle::TermThenKill => {
                kill_group(pid, libc::SIGTERM);
                if let Some(status) = reap_with_timeout(&mut child, Duration::from_secs(5))? {
                    status
                } else {
                    kill_group(pid, libc::SIGKILL);
                    child.wait()?
                }
            }
        }
    };
    let (stdout, stderr) = finish_readers(readers);
    let mut code = public_code(python_status(status));
    let interrupted = INTERRUPTED.swap(0, Ordering::SeqCst);
    if interrupted != 0 {
        // The handler already killed the group. The wrapper reports the
        // signal that arrived, and the deadline did not win.
        code = interrupted;
        timed_out = false;
    }
    Ok(Captured {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
    })
}

pub(super) fn launch_or_note(result: io::Result<Captured>) -> Result<Captured, i32> {
    match result {
        Ok(captured) => Ok(captured),
        Err(err) => {
            write_stderr_line(&format!("[crew] {err}"));
            Err(LAUNCH_EXIT)
        }
    }
}

fn spawn(argv: &[String], cwd: Option<&Path>, kind: StdioKind) -> io::Result<(Child, Readers)> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // SAFETY: setsid runs only in the forked child. It is async-signal-safe,
    // and it is what puts the provider in its own process group so a timeout
    // can signal grandchildren too.
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::setsid();
            if rc == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    match kind {
        StdioKind::Piped => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            let stdout = match child.stdout.take() {
                Some(pipe) => pipe,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::other("missing stdout pipe"));
                }
            };
            let stderr = match child.stderr.take() {
                Some(pipe) => pipe,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::other("missing stderr pipe"));
                }
            };
            let out = thread::spawn(move || read_pipe(stdout));
            let err = thread::spawn(move || read_pipe(stderr));
            Ok((child, Readers::Pipes { out, err }))
        }
        StdioKind::Pty => {
            let pty::PtyPair { master, slave } = pty::open_pty()?;
            set_cloexec(&master)?;
            set_cloexec(&slave)?;
            let stdin = dup_fd(&slave)?;
            let stdout = dup_fd(&slave)?;
            let stderr = dup_fd(&slave)?;
            drop(slave);
            cmd.stdin(Stdio::from(stdin))
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            let child = cmd.spawn()?;
            let stop = Arc::new(AtomicBool::new(false));
            let stop_reader = Arc::clone(&stop);
            let out = thread::spawn(move || read_pty(master, stop_reader));
            Ok((child, Readers::Pty { out, stop }))
        }
    }
}

enum Readers {
    Pipes {
        out: thread::JoinHandle<Vec<u8>>,
        err: thread::JoinHandle<Vec<u8>>,
    },
    Pty {
        out: thread::JoinHandle<Vec<u8>>,
        stop: Arc<AtomicBool>,
    },
}

fn finish_readers(readers: Readers) -> (Vec<u8>, Vec<u8>) {
    match readers {
        Readers::Pipes { out, err } => (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        ),
        Readers::Pty { out, stop } => {
            stop.store(true, Ordering::SeqCst);
            (out.join().unwrap_or_default(), Vec::new())
        }
    }
}

fn read_pipe(mut pipe: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf
}

fn read_pty(master: OwnedFd, stop: Arc<AtomicBool>) -> Vec<u8> {
    if set_nonblock(&master).is_err() {
        return read_pipe(File::from(master));
    }
    let mut file = File::from(master);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    loop {
        match file.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => break,
        }
    }
    buf
}

fn reap_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn python_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    if let Some(signal) = status.signal() {
        return -signal;
    }
    1
}

fn kill_group(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: killpg is async-signal-safe. A pid that already exited returns
    // ESRCH, which is ignored.
    unsafe {
        let _ = libc::killpg(pid, signal);
    }
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: the fd is open and owned.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_nonblock(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL on an owned fd. O_NONBLOCK is the only change.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn dup_fd(src: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new fd or -1. The new fd is owned here.
    let fd = unsafe { libc::fcntl(src.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

extern "C" fn on_signal(signal: libc::c_int) {
    INTERRUPTED.store(128i32.saturating_add(signal), Ordering::SeqCst);
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: killpg is async-signal-safe, which a signal handler requires.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

struct InterruptGuard {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

impl InterruptGuard {
    fn install(pid: u32) -> io::Result<Self> {
        let pid = i32::try_from(pid)
            .map_err(|_| io::Error::other("pid does not fit in a process group id"))?;
        CHILD_PID.store(pid, Ordering::SeqCst);
        let previous_int = match install_handler(libc::SIGINT) {
            Ok(previous) => previous,
            Err(err) => {
                CHILD_PID.store(0, Ordering::SeqCst);
                return Err(err);
            }
        };
        let previous_term = match install_handler(libc::SIGTERM) {
            Ok(previous) => previous,
            Err(err) => {
                restore_handler(libc::SIGINT, previous_int);
                CHILD_PID.store(0, Ordering::SeqCst);
                return Err(err);
            }
        };
        Ok(Self {
            previous_int,
            previous_term,
        })
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        CHILD_PID.store(0, Ordering::SeqCst);
        restore_handler(libc::SIGINT, self.previous_int);
        restore_handler(libc::SIGTERM, self.previous_term);
    }
}

fn install_handler(signal: i32) -> io::Result<libc::sigaction> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_signal as *const () as usize;
    action.sa_flags = 0;
    unsafe {
        // SAFETY: sigemptyset only writes the local mask. sigaction installs
        // `on_signal`, which is async-signal-safe, and saves the previous action.
        if libc::sigemptyset(std::ptr::addr_of_mut!(action.sa_mask)) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut previous: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, &action, &mut previous) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(previous)
    }
}

fn restore_handler(signal: i32, previous: libc::sigaction) {
    unsafe {
        // SAFETY: `previous` is the action sigaction returned for this signal.
        let _ = libc::sigaction(signal, &previous, std::ptr::null_mut());
    }
}

#[cfg(test)]
pub(super) fn bare(provider: Provider) -> Invocation {
    Invocation {
        provider,
        tier: None,
        model: None,
        effort: None,
        timeout_secs: 1800.0,
        cwd: None,
        resume: None,
        continue_session: false,
        prompt: Some("hi".to_string()),
        add_dir: Vec::new(),
        schema: None,
        raw: false,
        no_web: false,
        max_turns: None,
        agent: None,
        sandbox: false,
        review: false,
        uncommitted: false,
        base: None,
        commit: None,
    }
}

#[cfg(test)]
pub(super) fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    argv.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

#[cfg(test)]
pub(super) fn has(argv: &[String], name: &str) -> bool {
    argv.iter().any(|arg| arg == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dash_reads_stdin() {
        assert!(reads_process_stdin(None));
        assert!(reads_process_stdin(Some("-")));
        assert!(!reads_process_stdin(Some("hello")));
        assert!(!reads_process_stdin(Some("--")));

        let mut invocation = bare(Provider::Claude);
        invocation.prompt = Some("hello".to_string());
        assert_eq!(take_prompt(&invocation).unwrap().as_deref(), Some("hello"));
        invocation.prompt = Some("   ".to_string());
        assert_eq!(take_prompt(&invocation).unwrap_err().code, 2);

        let mut codex = bare(Provider::Codex);
        codex.review = true;
        codex.uncommitted = true;
        codex.prompt = None;
        assert_eq!(take_prompt(&codex).unwrap(), None);
    }

    #[test]
    fn schema_literal_is_not_read_as_a_path() {
        assert_eq!(load_schema_literal("  {\"a\":1}").unwrap(), "  {\"a\":1}");
        let err = load_schema_literal("crew-missing-schema-not-a-file.json").unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn pty_echo_is_captured() {
        let captured = capture(
            &["/bin/echo".to_string(), "hello-pty".to_string()],
            None,
            Duration::from_secs(5),
            StdioKind::Pty,
            KillStyle::TermThenKill,
            false,
        )
        .expect("pty echo");
        assert!(!captured.timed_out);
        assert_eq!(captured.code, 0);
        assert!(
            captured.stdout.contains("hello-pty"),
            "stdout was {:?}",
            captured.stdout
        );
    }

    #[test]
    fn timeout_kills_the_group() {
        let captured = capture(
            &["/bin/sleep".to_string(), "30".to_string()],
            None,
            Duration::from_millis(300),
            StdioKind::Piped,
            KillStyle::TermThenKill,
            false,
        )
        .expect("sleep");
        assert!(captured.timed_out);
    }
}
