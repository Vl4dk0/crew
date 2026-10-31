//! Spawn a provider child and turn its status into a crew exit code.
//!
//! The child inherits the environment. Stdin is `/dev/null` except the gemini
//! PTY, and the prompt is never written there. The child is its own session, so
//! a deadline or a SIGTERM delivered to this process does not reach it. Both
//! paths signal that session: SIGTERM, five seconds, SIGKILL, except codex,
//! which SIGKILLs at once.

use std::borrow::Cow;
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

#[derive(Debug)]
pub(crate) struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Id already written as `SESSION_ID=` while the child was still running.
    /// The final report must not print that same id again.
    pub session_id: Option<String>,
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
    verbatim: Option<&str>,
) -> (String, Option<String>) {
    // A config value is the wire word. Mapping it again would turn a
    // deliberate `max = "max"` back into ultra or xhigh.
    if let Some(wire) = verbatim {
        let note = (wire != effort).then(|| format!("[crew] effort {effort} -> {wire}"));
        return (wire.to_string(), note);
    }
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
        // pi's own scale already has max. xhigh stays off this CLI.
        (Provider::Pi, _) => "max",
    };
    let note = (wire != "max").then(|| format!("[crew] effort max -> {wire}"));
    (wire.to_string(), note)
}

/// Config remap for the command that is actually running.
///
/// Custom agents do not inherit `[effort.pi]`. The table key is their name.
pub(super) fn effort_verbatim(
    invocation: &Invocation,
    effort: &str,
) -> Result<Option<String>, Error> {
    let owner = invocation
        .user_agent
        .as_deref()
        .unwrap_or(invocation.provider.as_str());
    crate::tier::effort_override(owner, effort)
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

/// A stats field that the provider left out. `py_display` would say `None`,
/// which is a missing value, not a count, so the caller omits the field.
pub(super) fn shown(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        some => Some(py_display(some)),
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

/// Write the report. `announced` is the id capture already printed, so the
/// same `SESSION_ID` line is not written a second time. A different id, or
/// one capture never saw, is still printed here.
pub(super) fn emit_report(report: &Report, announced: Option<&str>) {
    write_raw_stderr(&without_session_line(&report.before_stderr, announced));
    write_raw_stdout(&report.stdout);
    write_raw_stderr(&without_session_line(&report.after_stderr, announced));
}

fn without_session_line<'a>(text: &'a str, announced: Option<&str>) -> Cow<'a, str> {
    let Some(id) = announced.filter(|id| !id.is_empty()) else {
        return Cow::Borrowed(text);
    };
    let target = format!("SESSION_ID={id}");
    if !text
        .split_inclusive('\n')
        .any(|chunk| is_session_line(chunk, &target))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive('\n') {
        if !is_session_line(chunk, &target) {
            out.push_str(chunk);
        }
    }
    Cow::Owned(out)
}

fn is_session_line(chunk: &str, target: &str) -> bool {
    chunk.trim_end_matches(['\n', '\r']) == target
}

/// The deadline still owns the exit code. Text and `SESSION_ID` already
/// parsed stay, so the killed run can be resumed and its answer is not
/// thrown away.
pub(super) fn keep_timeout(mut report: Report, timed_out: bool, secs: f64) -> Report {
    if !timed_out {
        return report;
    }
    report.code = 124;
    if !report.before_stderr.contains("timed out after")
        && !report.after_stderr.contains("timed out after")
    {
        report
            .after_stderr
            .push_str(&format!("[crew] timed out after {}s\n", format_secs(secs)));
    }
    report
}

/// Session id on one provider event line. Capture prints it as soon as the
/// line arrives, not after the process exits.
pub(super) fn session_from_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    for key in ["session_id", "sessionId", "thread_id"] {
        if let Some(id) = value.get(key).and_then(Value::as_str)
            && !id.is_empty()
        {
            return Some(id.to_string());
        }
    }
    None
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
    watch: &mut dyn FnMut(&str) -> Option<String>,
) -> io::Result<Captured> {
    drive(argv, cwd, timeout, kind, kill, watch, None)
}

/// Job steer writes the provider's own protocol on stdin and must stop when
/// the turn ends. Waiting for another prompt would hang the supervisor.
pub(super) struct Tick<'a> {
    pub lines: Vec<String>,
    /// Job stop already asked this process to die. Pi uses it to `abort`
    /// before the signal lands.
    pub stopping: bool,
    stdin: &'a mut Option<std::process::ChildStdin>,
}

impl Tick<'_> {
    pub(super) fn send(&mut self, line: &str) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let _ = stdin.write_all(line.as_bytes());
        if !line.ends_with('\n') {
            let _ = stdin.write_all(b"\n");
        }
        let _ = stdin.flush();
    }
}

pub(super) enum TickDone {
    Continue,
    /// `stop` kills a long-lived CLI (codex app-server) once the turn is done.
    /// That kill is not a user interrupt and not a timeout.
    Done {
        stop: bool,
    },
}

pub(super) fn capture_driven(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    kill: KillStyle,
    watch: &mut dyn FnMut(&str) -> Option<String>,
    mut on_tick: impl FnMut(&mut Tick<'_>) -> TickDone,
) -> io::Result<Captured> {
    drive(
        argv,
        cwd,
        timeout,
        StdioKind::Piped,
        kill,
        watch,
        Some(&mut on_tick),
    )
}

/// Note a new session id on stderr the moment `watch` can see it.
fn publish_session(
    text: &str,
    announced: &mut Option<String>,
    watch: &mut dyn FnMut(&str) -> Option<String>,
) {
    for line in text.lines() {
        note_session_line(line, announced, watch);
    }
    note_session_line("", announced, watch);
}

fn note_session_line(
    line: &str,
    announced: &mut Option<String>,
    watch: &mut dyn FnMut(&str) -> Option<String>,
) {
    let Some(id) = watch(line) else {
        return;
    };
    if announced.as_deref() == Some(id.as_str()) {
        return;
    }
    write_stderr_line(&format!("SESSION_ID={id}"));
    *announced = Some(id);
}

/// One wait for a normal capture and for a job whose stdin carries steer.
///
/// `on_tick` is set only for steer. `Done { stop: true }` kills a long-lived
/// CLI once the turn is over. That kill is not a timeout and not a user signal.
/// detach_session makes the provider a session leader, so it is not in this
/// process group. Without a handler, SIGTERM (job stop) takes the default
/// action, this process dies, and that session is reparented to init.
fn drive(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    kind: StdioKind,
    kill: KillStyle,
    watch: &mut dyn FnMut(&str) -> Option<String>,
    mut on_tick: Option<&mut dyn FnMut(&mut Tick<'_>) -> TickDone>,
) -> io::Result<Captured> {
    let _lock = CAPTURE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    INTERRUPTED.store(0, Ordering::SeqCst);

    // A tick writes the provider protocol, so stdin is a pipe only then.
    let (mut child, readers, mut stdin) = spawn(argv, cwd, kind, on_tick.is_some())?;
    let pid = child.id();
    let _guard = match InterruptGuard::install() {
        Ok(guard) => guard,
        Err(err) => {
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
            return Err(err);
        }
    };

    let mut announced = None;
    let mut scanned = 0usize;
    let mut timed_out = false;
    let mut self_stop = false;
    let start = Instant::now();
    let status = loop {
        let lines = drain_new_lines(&readers.stdout, &mut scanned, &mut announced, watch);
        let stopping = INTERRUPTED.load(Ordering::SeqCst) != 0;
        if let Some(tick_fn) = on_tick.as_mut()
            && stdin.is_some()
        {
            match tick_fn(&mut Tick {
                lines,
                stopping,
                stdin: &mut stdin,
            }) {
                TickDone::Continue => {}
                TickDone::Done { stop } => {
                    stdin.take();
                    if stop {
                        self_stop = true;
                        break None;
                    }
                }
            }
        }
        if let Some(status) = child.try_wait()? {
            let _ = drain_new_lines(&readers.stdout, &mut scanned, &mut announced, watch);
            break Some(status);
        }
        // SIGTERM is only recorded here. stop_group snapshots the descendant
        // tree before anyone is signaled, so this wait must not be cut short.
        if stopping {
            break None;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let status = if self_stop {
        stop_group(
            &mut child,
            kill,
            &readers.stdout,
            &mut scanned,
            &mut announced,
            watch,
        )?
    } else {
        match status {
            Some(status) if INTERRUPTED.load(Ordering::SeqCst) == 0 && !timed_out => status,
            other => {
                if other.is_none() && INTERRUPTED.load(Ordering::SeqCst) == 0 {
                    timed_out = true;
                }
                stop_group(
                    &mut child,
                    kill,
                    &readers.stdout,
                    &mut scanned,
                    &mut announced,
                    watch,
                )?
            }
        }
    };
    let (stdout, stderr) = finish_readers(readers);
    let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
    publish_session(&stdout_text, &mut announced, watch);
    let mut code = public_code(python_status(status));
    let user_stop = INTERRUPTED.swap(0, Ordering::SeqCst);
    if user_stop != 0 {
        // Job stop and Ctrl-C report the signal, not the deadline.
        code = user_stop;
        timed_out = false;
    } else if self_stop {
        // The turn already finished. Stopping the long-lived CLI is not a
        // failed run; the caller's success check still decides 0 or 125.
        timed_out = false;
        code = 0;
    }
    Ok(Captured {
        code,
        stdout: stdout_text,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
        session_id: announced,
    })
}

/// Finish a deadline or a forwarded signal.
///
/// The provider is a session leader, and a tool it starts may call setsid
/// again. killpg on the provider misses that process, and once the provider
/// exits the tool is reparented to init. Name every descendant first, then
/// signal that list and the provider's group.
fn stop_group(
    child: &mut Child,
    kill: KillStyle,
    stdout: &Mutex<Vec<u8>>,
    scanned: &mut usize,
    announced: &mut Option<String>,
    watch: &mut dyn FnMut(&str) -> Option<String>,
) -> io::Result<ExitStatus> {
    let pid = child.id();
    let (first, grace) = match kill {
        KillStyle::ImmediateKill => (libc::SIGKILL, false),
        KillStyle::TermThenKill => (libc::SIGTERM, true),
    };
    let mut marked = signal_tree(pid, first);
    if grace {
        let start = Instant::now();
        loop {
            let _lines = drain_new_lines(stdout, scanned, announced, watch);
            let status = child.try_wait()?;
            if let Some(status) = status
                && tree_dead(pid, &marked)
            {
                return Ok(status);
            }
            if start.elapsed() >= Duration::from_secs(5) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        marked = signal_tree(pid, libc::SIGKILL);
    }
    let status = reap_child(child)?;
    let start = Instant::now();
    while !tree_dead(pid, &marked) && start.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(20));
    }
    Ok(status)
}

fn tree_dead(root: u32, marked: &[u32]) -> bool {
    !group_alive(root) && marked.iter().all(|pid| !process_alive(*pid))
}

/// Descendants first, then `root`, then whatever is still in `root`'s group.
fn signal_tree(root: u32, signal: i32) -> Vec<u32> {
    let mut marked = Vec::new();
    collect_descendants(root, &mut marked);
    for pid in &marked {
        signal_pid(*pid, signal);
    }
    signal_pid(root, signal);
    kill_group(root, signal);
    marked.push(root);
    marked
}

fn collect_descendants(pid: u32, out: &mut Vec<u32>) {
    for child in direct_children(pid) {
        if child <= 1 || out.contains(&child) {
            continue;
        }
        collect_descendants(child, out);
        out.push(child);
    }
}

fn signal_pid(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // SAFETY: kill is async-signal-safe. A pid that already exited returns ESRCH.
    unsafe {
        libc::kill(pid, signal);
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes. ESRCH means the pid is gone.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "macos")]
fn direct_children(parent: u32) -> Vec<u32> {
    let Ok(parent) = i32::try_from(parent) else {
        return Vec::new();
    };
    // SAFETY: a null buffer asks libproc how many child pids exist.
    let needed = unsafe { proc_listchildpids(parent, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0i32; needed as usize];
    let bytes = match i32::try_from(buf.len().saturating_mul(std::mem::size_of::<i32>())) {
        Ok(bytes) => bytes,
        Err(_) => return Vec::new(),
    };
    // SAFETY: buf is writable for `bytes` bytes. The return is a pid count.
    let got = unsafe { proc_listchildpids(parent, buf.as_mut_ptr(), bytes) };
    if got <= 0 {
        return Vec::new();
    }
    let got = (got as usize).min(buf.len());
    buf.truncate(got);
    buf.into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .collect()
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_listchildpids(ppid: i32, buffer: *mut i32, buffersize: i32) -> i32;
}

#[cfg(target_os = "linux")]
fn direct_children(parent: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let ppid = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|rest| rest.trim().parse::<u32>().ok())
        });
        if ppid == Some(parent) && pid > 1 {
            children.push(pid);
        }
    }
    children
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn direct_children(_parent: u32) -> Vec<u32> {
    Vec::new()
}

fn reap_child(child: &mut Child) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    child.wait()
}

fn drain_new_lines(
    stdout: &Mutex<Vec<u8>>,
    scanned: &mut usize,
    announced: &mut Option<String>,
    watch: &mut dyn FnMut(&str) -> Option<String>,
) -> Vec<String> {
    let fresh = {
        let buf = stdout.lock().unwrap_or_else(|err| err.into_inner());
        if buf.len() <= *scanned {
            Vec::new()
        } else {
            buf[*scanned..].to_vec()
        }
    };
    let mut lines = Vec::new();
    if let Some(end) = fresh.iter().rposition(|byte| *byte == b'\n') {
        let complete = String::from_utf8_lossy(&fresh[..=end]);
        *scanned += end + 1;
        for line in complete.lines() {
            note_session_line(line, announced, watch);
            lines.push(line.to_string());
        }
    }
    // Providers that learn the id from outside the stream (agy's conversation
    // file) get a look even when the child is quiet.
    note_session_line("", announced, watch);
    lines
}

/// Bound a version check or an updater the same way a provider run is bounded.
///
/// Stdin is `/dev/null` so a confirmation prompt cannot block, and the
/// deadline kills the process group. A self-update that is still prompting
/// or downloading must not hold `crew update` open.
pub(crate) fn run_bounded(argv: &[String], timeout: Duration) -> io::Result<Captured> {
    capture(
        argv,
        None,
        timeout,
        StdioKind::Piped,
        KillStyle::TermThenKill,
        &mut |_| None,
    )
}

/// Check if an executable binary is present on PATH.
pub fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        std::fs::metadata(&candidate).is_ok_and(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Report a launch failure to stderr and map to the exit code.
///
/// Missing binaries on PATH are distinguished from other OS errors so
/// the user is told directly which CLI needs to be installed.
pub(super) fn launch_or_note(bin: &str, result: io::Result<Captured>) -> Result<Captured, i32> {
    match result {
        Ok(captured) => Ok(captured),
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound || !binary_on_path(bin) {
                write_stderr_line(&format!("[crew] {bin} CLI not found on PATH"));
            } else {
                write_stderr_line(&format!("[crew] {err}"));
            }
            Err(LAUNCH_EXIT)
        }
    }
}

struct Readers {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    handles: ReaderHandles,
}

enum ReaderHandles {
    Pipes {
        out: thread::JoinHandle<()>,
        err: thread::JoinHandle<()>,
    },
    Pty {
        out: thread::JoinHandle<()>,
        stop: Arc<AtomicBool>,
    },
}

fn give_up(child: &mut Child, which: &str) -> io::Error {
    let _ = child.kill();
    let _ = child.wait();
    io::Error::other(format!("missing {which} pipe"))
}

fn spawn(
    argv: &[String],
    cwd: Option<&Path>,
    kind: StdioKind,
    pipe_stdin: bool,
) -> io::Result<(Child, Readers, Option<std::process::ChildStdin>)> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // A job sets this so the wrapper can see the inbox. The provider CLI and
    // any tool it spawns must not inherit it.
    cmd.env_remove("CREW_JOB");
    detach_session(&mut cmd);
    match kind {
        StdioKind::Piped => {
            cmd.stdin(if pipe_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            let stdin = child.stdin.take();
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| give_up(&mut child, "stdout"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| give_up(&mut child, "stderr"))?;
            let stdout_buf = Arc::new(Mutex::new(Vec::new()));
            let stderr_buf = Arc::new(Mutex::new(Vec::new()));
            let out = thread::spawn({
                let stdout_buf = Arc::clone(&stdout_buf);
                move || read_pipe_into(stdout, &stdout_buf)
            });
            let err = thread::spawn({
                let stderr_buf = Arc::clone(&stderr_buf);
                move || read_pipe_into(stderr, &stderr_buf)
            });
            Ok((
                child,
                Readers {
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                    handles: ReaderHandles::Pipes { out, err },
                },
                stdin,
            ))
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
            let stdout_buf = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let stop_reader = Arc::clone(&stop);
            let out = thread::spawn({
                let stdout_buf = Arc::clone(&stdout_buf);
                move || read_pty_into(master, stop_reader, &stdout_buf)
            });
            Ok((
                child,
                Readers {
                    stdout: stdout_buf,
                    stderr: Arc::new(Mutex::new(Vec::new())),
                    handles: ReaderHandles::Pty { out, stop },
                },
                None,
            ))
        }
    }
}

fn finish_readers(readers: Readers) -> (Vec<u8>, Vec<u8>) {
    match readers.handles {
        ReaderHandles::Pipes { out, err } => {
            let _ = out.join();
            let _ = err.join();
        }
        ReaderHandles::Pty { out, stop } => {
            stop.store(true, Ordering::SeqCst);
            let _ = out.join();
        }
    }
    let stdout = readers
        .stdout
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let stderr = readers
        .stderr
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    (stdout, stderr)
}

fn read_pipe_into(mut pipe: impl Read, dest: &Mutex<Vec<u8>>) {
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => dest
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend_from_slice(&tmp[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn read_pty_into(master: OwnedFd, stop: Arc<AtomicBool>, dest: &Mutex<Vec<u8>>) {
    if set_nonblock(&master).is_err() {
        read_pipe_into(File::from(master), dest);
        return;
    }
    let mut file = File::from(master);
    let mut tmp = [0u8; 65536];
    loop {
        match file.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => dest
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend_from_slice(&tmp[..n]),
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

/// New session for the child, so its descendants share a process group the
/// caller can signal after the child itself has exited.
pub(crate) fn detach_session(cmd: &mut Command) {
    // SAFETY: setsid runs only in the forked child. It is async-signal-safe,
    // and it is what puts the child in its own process group so a later
    // killpg reaches descendants that stay in that group.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// SIGTERM the child's process group, then SIGKILL if anyone is still in it.
/// The direct child is reaped here. Descendants that outlive it are the point:
/// a quota probe can exit while the servers it spawned keep the group occupied.
pub(crate) fn terminate_group(child: &mut Child) {
    let pid = child.id();
    kill_group(pid, libc::SIGTERM);
    let start = Instant::now();
    loop {
        let reaped = matches!(child.try_wait(), Ok(Some(_)));
        if reaped && !group_alive(pid) {
            return;
        }
        if start.elapsed() >= Duration::from_secs(5) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    kill_group(pid, libc::SIGKILL);
    let _ = child.wait();
}

fn group_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: killpg with signal 0 only probes membership. ESRCH means the
    // group is gone; any other result means someone is still in it.
    let rc = unsafe { libc::killpg(pid, 0) };
    if rc == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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
    // Only the flag. killpg here would reap the provider before the main
    // thread can list descendants that have their own session, and those
    // would be reparented to init with nothing left pointing at them.
    INTERRUPTED.store(128i32.saturating_add(signal), Ordering::SeqCst);
}

struct InterruptGuard {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

impl InterruptGuard {
    fn install() -> io::Result<Self> {
        let previous_int = install_handler(libc::SIGINT)?;
        let previous_term = match install_handler(libc::SIGTERM) {
            Ok(previous) => previous,
            Err(err) => {
                restore_handler(libc::SIGINT, previous_int);
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
        timeout_secs: crate::cli::DEFAULT_TIMEOUT_SECS,
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
        user_agent: None,
    }
}

/// Model value from `-m` / `--model` on an argv this crate built.
pub(crate) fn model_arg(argv: &[String]) -> Option<String> {
    argv.windows(2)
        .find(|pair| pair[0] == "-m" || pair[0] == "--model")
        .map(|pair| pair[1].clone())
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
            &mut |_| None,
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
            &mut |_| None,
        )
        .expect("sleep");
        assert!(captured.timed_out);
    }

    #[test]
    fn sigterm_kills_the_detached_session() {
        // The child is a session leader, same as a provider CLI. Its perl
        // grandchild calls setsid, which is what a provider tool process does,
        // so it is not in the child's group. SIGTERM to us must reap both.
        let path = std::env::temp_dir().join(format!("crew-sigterm-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let script = r#"echo $$ > "$1"; perl -e 'use POSIX (); POSIX::setsid(); open my $f, ">>", shift; print $f "$$\n"; close $f; sleep 60' "$1" & wait"#;
        let path_arg = path.to_string_lossy().to_string();
        let signaler = thread::spawn({
            let path = path.clone();
            move || {
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(3) {
                    if let Ok(text) = fs::read_to_string(&path) {
                        let ready = text.lines().filter(|line| !line.trim().is_empty()).count();
                        if ready >= 2 {
                            unsafe {
                                libc::kill(std::process::id() as i32, libc::SIGTERM);
                            }
                            return;
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        let captured = capture(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                script.to_string(),
                "sh".to_string(),
                path_arg,
            ],
            None,
            Duration::from_secs(5),
            StdioKind::Piped,
            KillStyle::TermThenKill,
            &mut |_| None,
        )
        .expect("capture");
        let _ = signaler.join();

        assert!(!captured.timed_out, "code {}", captured.code);
        assert_eq!(captured.code, 128 + libc::SIGTERM);

        let text = fs::read_to_string(&path).expect("pid file");
        let pids: Vec<u32> = text
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        assert_eq!(pids.len(), 2, "{text:?}");
        assert_ne!(pids[0], pids[1]);
        assert!(pids.iter().all(|pid| *pid != std::process::id()));

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) && pids.iter().any(|pid| process_alive(*pid))
        {
            thread::sleep(Duration::from_millis(20));
        }
        let live: Vec<u32> = pids.into_iter().filter(|pid| process_alive(*pid)).collect();
        let _ = fs::remove_file(&path);
        assert!(live.is_empty(), "detached session still running: {live:?}");
    }

    #[test]
    fn final_report_omits_the_session_capture_already_announced() {
        let text = "[crew] note\nSESSION_ID=abc\n[crew] usage\n";
        assert_eq!(
            without_session_line(text, Some("abc")).as_ref(),
            "[crew] note\n[crew] usage\n"
        );
        assert_eq!(without_session_line(text, Some("other")).as_ref(), text);
        assert_eq!(without_session_line(text, None).as_ref(), text);
        assert_eq!(
            without_session_line("SESSION_ID=abc\n", Some("abc")).as_ref(),
            ""
        );
    }

    #[test]
    fn missing_cli_on_path_returns_launch_exit() {
        // Missing binary on PATH must map to exit code 127
        let res = launch_or_note(
            "missing_binary_xyz",
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No such file or directory",
            )),
        );
        assert_eq!(res.unwrap_err(), LAUNCH_EXIT);
        assert!(!binary_on_path("missing_binary_xyz"));
    }
}
