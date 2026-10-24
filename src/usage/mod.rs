#![allow(clippy::collapsible_if)]

mod plan;
pub mod render;

use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::providers::{detach_session, terminate_group};

use serde_json::Value;

use crate::error::Error;
use render::{Gauge, ProviderView, render_blocks};

/// Where probe state lives. Matches the directory the macOS UsageBar app uses;
/// Linux has no Application Support, so it follows the XDG base dir spec.
fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("USAGEBAR_STATE") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "linux") {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|x| !x.is_empty()) {
            return PathBuf::from(xdg).join("UsageBar");
        }
        return home_dir().join(".local/share/UsageBar");
    }
    home_dir().join("Library/Application Support/UsageBar")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Every provider reports the same way: gauges, or a single line saying why not.
fn provider_view(
    key: &str,
    name: &str,
    mark: &str,
    windows: Vec<Gauge>,
    error: Option<String>,
) -> ProviderView {
    let error = error.or_else(|| {
        windows
            .is_empty()
            .then(|| "failed to load usage".to_string())
    });
    ProviderView {
        key: key.to_string(),
        name: name.to_string(),
        mark: mark.to_string(),
        plan: None,
        windows,
        error,
    }
}

fn percent(value: f64) -> u32 {
    value.round().clamp(0.0, 100.0) as u32
}

fn gauge(
    id: &str,
    label: impl Into<String>,
    percent: u32,
    reset: Option<String>,
    window_secs: Option<u64>,
) -> Gauge {
    Gauge {
        id: id.to_string(),
        label: label.into(),
        percent,
        reset,
        window_secs,
    }
}

/// GET with curl, returning the body and HTTP status. `None` when curl failed
/// or printed no status line.
fn curl_get(url: &str, headers: &[String]) -> Option<(String, u16)> {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-m", "10", "-w", "\n%{http_code}"]);
    for header in headers {
        cmd.args(["-H", header]);
    }
    let out = cmd.arg(url).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.trim().rsplit_once('\n')?;
    Some((body.to_string(), code.parse().unwrap_or(0)))
}

fn is_auth_failure(code: u16) -> bool {
    code == 401 || code == 403
}

/// Run `cmd` on a pty and collect its output. `agy` starts MCP servers as
/// grandchildren, so the command gets its own session and the whole group is
/// signalled on the way out, not only the direct child.
fn run_on_pty(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, Error> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    detach_session(&mut cmd);
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(Error::new(2, "failed to openpty"));
    }

    let slave_file = unsafe { fs::File::from_raw_fd(slave) };
    let slave_file_err = slave_file
        .try_clone()
        .map_err(|e| Error::new(2, format!("clone slave failed: {}", e)))?;

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(slave_file));
    cmd.stderr(Stdio::from(slave_file_err));

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::new(2, format!("failed to spawn on pty: {}", e)))?;

    let mut master_file = unsafe { fs::File::from_raw_fd(master) };
    let master_fd = master_file.as_raw_fd();

    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL, 0);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let start = Instant::now();
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        let mut fds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::FD_SET(master_fd, &mut fds);
        }
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 100_000,
        };
        let ready = unsafe {
            libc::select(
                master_fd + 1,
                &mut fds,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut tv,
            )
        };
        if ready > 0 {
            match master_file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if let Ok(Some(_)) = child.try_wait() {
            while let Ok(n) = master_file.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buf[..n]);
            }
            break;
        }
    }

    terminate_group(&mut child);
    Ok(output)
}

fn codex_window(key: &str, win: &Value) -> Option<Gauge> {
    let used = win["used_percent"].as_f64()?;
    let seconds = win["limit_window_seconds"].as_i64().unwrap_or(0);
    let window = if (1..48 * 3600).contains(&seconds) {
        format!("{}h", (seconds as f64 / 3600.0).round() as i64)
    } else {
        "week".to_string()
    };
    let reset = match &win["reset_at"] {
        Value::Number(epoch) => Some(epoch.to_string()),
        other => other.as_str().map(str::to_string),
    };
    Some(gauge(
        &format!("codex:{key}"),
        format!("Codex · {window}"),
        percent(used),
        reset,
        u64::try_from(seconds).ok().filter(|s| *s > 0),
    ))
}

pub(crate) fn parse_codex_rate_limit(resp: &Value) -> Vec<Gauge> {
    ["primary_window", "secondary_window"]
        .into_iter()
        .filter_map(|key| codex_window(key, &resp["rate_limit"][key]))
        .collect()
}

/// Both CLIs keep their OAuth state in a JSON file under the home directory;
/// a missing, malformed or empty one means the user is not logged in.
fn read_auth_file(home: &Path, rel: &str) -> Result<Value, &'static str> {
    let content = fs::read_to_string(home.join(rel)).map_err(|_| NOT_LOGGED_IN)?;
    serde_json::from_str(&content).map_err(|_| NOT_LOGGED_IN)
}

const NOT_LOGGED_IN: &str = "not logged in";

fn non_blank(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.trim().is_empty())
}

fn codex_token(home: &Path) -> Result<(String, Option<String>), &'static str> {
    let val = read_auth_file(home, ".codex/auth.json")?;
    let token = non_blank(&val["tokens"]["access_token"]).ok_or(NOT_LOGGED_IN)?;
    let account_id = non_blank(&val["tokens"]["account_id"]).map(str::to_string);
    Ok((token.to_string(), account_id))
}

fn fetch_codex() -> ProviderView {
    let (windows, error) = match codex_token(&home_dir()) {
        Err(err) => (Vec::new(), Some(err.to_string())),
        Ok((token, account_id)) => {
            let mut headers = vec![
                format!("Authorization: Bearer {token}"),
                "User-Agent: UsageBar".to_string(),
                "Content-Type: application/json".to_string(),
            ];
            headers.extend(account_id.map(|acc| format!("ChatGPT-Account-Id: {acc}")));
            match curl_get("https://chatgpt.com/backend-api/wham/usage", &headers) {
                Some((_, code)) if is_auth_failure(code) => {
                    (Vec::new(), Some(NOT_LOGGED_IN.to_string()))
                }
                Some((body, _)) => {
                    let resp = serde_json::from_str(&body).unwrap_or(Value::Null);
                    (parse_codex_rate_limit(&resp), None)
                }
                None => (Vec::new(), None),
            }
        }
    };
    provider_view("codex", "codex", "⚛", windows, error)
}

pub(crate) fn is_agy_unauthenticated(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not authenticated")
        || lower.contains("you are not logged into antigravity")
        || lower.contains("unauthenticated")
}

/// agy is slow to start, so a recent answer is reused instead of re-probed.
const ANTIGRAVITY_CACHE_TTL: Duration = Duration::from_secs(120);

fn read_fresh_cache(path: &Path) -> Option<Value> {
    let age = SystemTime::now()
        .duration_since(fs::metadata(path).ok()?.modified().ok()?)
        .ok()?;
    if age >= ANTIGRAVITY_CACHE_TTL {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// `agy` needs the network to answer, and without it hangs until the timeout.
fn google_reachable() -> bool {
    "oauth2.googleapis.com:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok())
}

/// Ask `agy` for usage. `Err` carries the user-facing reason; `Ok(None)` means
/// no usable answer.
fn probe_agy(cache_file: &Path) -> Result<Option<Value>, &'static str> {
    if !google_reachable() {
        return Ok(None);
    }
    let mut cmd = Command::new("agy");
    cmd.args(["-p", "/usage", "--output-format", "json"]);
    let Ok(output) = run_on_pty(cmd, Duration::from_secs(25)) else {
        return Ok(None);
    };
    if is_agy_unauthenticated(&String::from_utf8_lossy(&output)) {
        return Err(NOT_LOGGED_IN);
    }
    let Ok(parsed) = serde_json::from_slice(&output) else {
        return Ok(None);
    };
    let _ = fs::write(cache_file, &output);
    Ok(Some(parsed))
}

fn antigravity_label(group: &str, window: &str) -> String {
    let five_hour = window == "5h";
    if group.contains("Gemini Models") || group == "Gemini" {
        format!("gemini {}", if five_hour { "5h" } else { "week" })
    } else if group.contains("Claude") || group.contains("Other") {
        format!("other models {}", if five_hour { "5h" } else { "week" })
    } else if group.contains("Argon") {
        "Gemini 4 Argon week".to_string()
    } else {
        format!("{group} {window}")
    }
}

fn parse_antigravity(root: &Value) -> Vec<Gauge> {
    // The cache is shared with the UsageBar app, which stores it unwrapped.
    let groups = [&root["command"]["data"], &root["data"], root]
        .into_iter()
        .find_map(|node| node["groups"].as_array());
    let buckets = groups.into_iter().flatten().flat_map(|group| {
        let name = group["name"].as_str().unwrap_or("");
        group["buckets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(move |bucket| (name, bucket))
    });
    buckets
        .map(|(group, bucket)| {
            let window = bucket["window"].as_str().unwrap_or("");
            let remaining = bucket["remaining_fraction"].as_f64().unwrap_or(1.0);
            let window_secs = match window {
                "5h" => Some(5 * 3600),
                "weekly" => Some(7 * 24 * 3600),
                _ => None,
            };
            gauge(
                &format!("antigravity:{}", bucket["id"].as_str().unwrap_or("")),
                antigravity_label(group, window),
                percent((1.0 - remaining) * 100.0),
                bucket["reset_time"].as_str().map(str::to_string),
                window_secs,
            )
        })
        .collect()
}

fn fetch_gemini() -> ProviderView {
    let cache_file = state_dir().join("antigravity_cache.json");
    let (root, error) = match read_fresh_cache(&cache_file) {
        Some(cached) => (Some(cached), None),
        None => match probe_agy(&cache_file) {
            Ok(root) => (root, None),
            Err(err) => (None, Some(err.to_string())),
        },
    };
    let windows = root.as_ref().map(parse_antigravity).unwrap_or_default();
    provider_view("gemini", "antigravity", "✦", windows, error)
}

/// Best OIDC token in ~/.grok/auth.json (latest expiry), with that expiry.
fn grok_token(home: &Path) -> Result<(String, Option<String>), &'static str> {
    let data = read_auth_file(home, ".grok/auth.json")?;
    let entries = data.as_object().ok_or(NOT_LOGGED_IN)?;
    entries
        .values()
        .rev()
        .filter(|entry| entry["auth_mode"] == "oidc")
        .filter_map(|entry| {
            let expiry = entry["expires_at"].as_str().unwrap_or("");
            Some((non_blank(&entry["key"])?, expiry))
        })
        .max_by_key(|(_, expiry)| *expiry)
        .map(|(key, expiry)| {
            (
                key.to_string(),
                Some(expiry.to_string()).filter(|e| !e.is_empty()),
            )
        })
        .ok_or(NOT_LOGGED_IN)
}

/// Running any grok command makes the CLI refresh its own token on disk.
fn refresh_grok_token() {
    let _ = Command::new("grok")
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
}

fn grok_billing(token: &str, query: &str) -> Option<(Value, u16)> {
    let headers = [
        format!("Authorization: Bearer {token}"),
        "X-XAI-Token-Auth: xai-grok-cli".to_string(),
    ];
    let url = format!("https://cli-chat-proxy.grok.com/v1/billing{query}");
    let (body, code) = curl_get(&url, &headers)?;
    Some((serde_json::from_str(&body).ok()?, code))
}

/// Billing answers either bare or wrapped in `config`.
fn billing_config(resp: &Value) -> &Value {
    resp.get("config").unwrap_or(resp)
}

/// Amounts arrive either bare or as `{ "val": n }`.
fn amount(value: &Value) -> Option<f64> {
    value["val"].as_f64().or_else(|| value.as_f64())
}

fn is_weekly_period(cfg: &Value) -> bool {
    cfg["currentPeriod"]["type"]
        .as_str()
        .is_some_and(|kind| kind.to_uppercase().contains("WEEK"))
}

fn weekly_credit_percent(cfg: &Value) -> Option<f64> {
    cfg["creditUsagePercent"]
        .as_f64()
        .or_else(|| {
            cfg["productUsage"].as_array()?.iter().find_map(|item| {
                let name = item["product"]
                    .as_str()?
                    .to_lowercase()
                    .replace(['-', '_'], "");
                (name == "grokbuild" || name == "build")
                    .then(|| item["usagePercent"].as_f64())
                    .flatten()
            })
        })
        .or_else(|| is_weekly_period(cfg).then_some(0.0))
}

fn grok_weekly_gauge(cfg: &Value) -> Option<Gauge> {
    let used = weekly_credit_percent(cfg)?;
    let period = &cfg["currentPeriod"];
    let reset = period["end"]
        .as_str()
        .or_else(|| cfg["billingPeriodEnd"].as_str())
        .map(str::to_string);
    let window_secs = period_window_secs(
        period["start"].as_str(),
        period["end"].as_str(),
        is_weekly_period(cfg) && reset.is_some(),
    );
    Some(gauge(
        "grok:weekly",
        "week",
        percent(used),
        reset,
        window_secs,
    ))
}

fn grok_monthly_gauge(cfg: &Value) -> Option<Gauge> {
    let limit = amount(&cfg["monthlyLimit"]).filter(|l| *l > 0.0)?;
    let used = amount(&cfg["used"])?;
    let period = &cfg["currentPeriod"];
    Some(gauge(
        "grok:monthly",
        "month",
        percent(used / limit * 100.0),
        cfg["billingPeriodEnd"].as_str().map(str::to_string),
        period_window_secs(period["start"].as_str(), period["end"].as_str(), false),
    ))
}

/// Pay-as-you-go spend against its cap, from whichever billing view has a cap.
fn grok_ondemand_gauge(cfgs: &[&Value]) -> Option<Gauge> {
    let cfg = cfgs
        .iter()
        .find(|cfg| amount(&cfg["onDemandCap"]).is_some())?;
    let cap = amount(&cfg["onDemandCap"]).filter(|c| *c > 0.0)?;
    let spent = amount(&cfg["onDemandUsed"])?;
    let reset = cfg["billingPeriodEnd"]
        .as_str()
        .or_else(|| cfg["currentPeriod"]["end"].as_str())
        .map(str::to_string);
    Some(gauge(
        "grok:ondemand",
        "pay-as-you-go",
        percent(spent / cap * 100.0),
        reset,
        None,
    ))
}

/// The token is refreshed up front when it is about to expire, and once more if
/// the API rejects it.
fn fresh_grok_token(home: &Path, token: String, expiry: Option<String>) -> Option<String> {
    let expiring = expiry
        .as_deref()
        .and_then(crate::job::parse_iso_time)
        .is_some_and(|at| {
            at.duration_since(SystemTime::now())
                .map_or(true, |left| left < Duration::from_secs(120))
        });
    if !expiring {
        return Some(token);
    }
    refresh_grok_token();
    grok_token(home).ok().map(|(token, _)| token)
}

fn fetch_grok() -> ProviderView {
    let home = home_dir();
    let (windows, error) = match grok_token(&home) {
        Err(err) => (Vec::new(), Some(err.to_string())),
        Ok((token, expiry)) => grok_windows(&home, fresh_grok_token(&home, token, expiry)),
    };
    provider_view("grok", "grok", "✕", windows, error)
}

fn grok_windows(home: &Path, token: Option<String>) -> (Vec<Gauge>, Option<String>) {
    let Some(mut token) = token else {
        return (Vec::new(), None);
    };
    let mut weekly = grok_billing(&token, "?format=credits");
    if matches!(weekly, Some((_, code)) if is_auth_failure(code)) {
        refresh_grok_token();
        if let Ok((renewed, _)) = grok_token(home) {
            if renewed != token {
                token = renewed;
                weekly = grok_billing(&token, "?format=credits");
            }
        }
    }
    let rejected = matches!(weekly, Some((_, code)) if is_auth_failure(code));
    let monthly = if rejected {
        None
    } else {
        grok_billing(&token, "")
    };

    let weekly_cfg = weekly.as_ref().map(|(resp, _)| billing_config(resp));
    let monthly_cfg = monthly
        .as_ref()
        .filter(|(_, code)| *code == 200)
        .map(|(resp, _)| billing_config(resp));

    let windows = [
        weekly_cfg.and_then(grok_weekly_gauge),
        monthly_cfg.and_then(grok_monthly_gauge),
        grok_ondemand_gauge(
            &[weekly_cfg, monthly_cfg]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    (windows, rejected.then(|| NOT_LOGGED_IN.to_string()))
}

/// Measured start/end when both parse. A weekly period type is 7 days only
/// when the payload says so and an end exists; a missing start is not guessed.
fn period_window_secs(start: Option<&str>, end: Option<&str>, weekly: bool) -> Option<u64> {
    let span = start
        .and_then(render::reset_instant)
        .zip(end.and_then(render::reset_instant))
        .and_then(|(start, end)| end.duration_since(start).ok())
        .map(|span| span.as_secs())
        .filter(|secs| *secs > 0);
    span.or(weekly.then_some(7 * 24 * 3600))
}

/// Claude Code reports a missing login either as an error reply on the control
/// channel or as `loggedIn: false`.
pub(crate) fn claude_probe_says_no_auth(val: &Value) -> bool {
    if val["type"] == "control_response" && val["response"]["subtype"] == "error" {
        let msg = val["response"]["error"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        return msg.is_empty()
            || [
                "auth",
                "login",
                "logged in",
                "unauthorized",
                "token",
                "credential",
            ]
            .iter()
            .any(|word| msg.contains(word));
    }
    val["loggedIn"] == false
}

/// Claude Code answers `get_usage` on its stream-json control channel with the
/// exact utilization and an ISO `resets_at` per quota window, which the
/// `/usage` screen only shows as prose. That timestamp is what makes the
/// api-dollar window start exact.
fn run_claude_usage_probe() -> Result<Vec<Gauge>, Error> {
    let probe_dir = state_dir().join("ClaudeProbe");
    let _ = fs::create_dir_all(&probe_dir);

    let mut child = Command::new("claude")
        .args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--setting-sources",
            "",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .current_dir(&probe_dir)
        .env("DISABLE_AUTOUPDATER", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_SESSION_ACCESS_TOKEN")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::new(2, format!("failed to spawn claude probe: {e}")))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let requests = concat!(
        r#"{"type":"control_request","request_id":"initialize","request":{"subtype":"initialize"}}"#,
        "\n",
        r#"{"type":"control_request","request_id":"usage","request":{"subtype":"get_usage","skip_behaviors":true}}"#,
        "\n",
    );
    let sent = stdin
        .write_all(requests.as_bytes())
        .and_then(|_| stdin.flush());

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(val) = serde_json::from_str::<Value>(&line) {
                let is_usage_reply =
                    val["type"] == "control_response" && val["response"]["request_id"] == "usage";
                if is_usage_reply || claude_probe_says_no_auth(&val) {
                    let _ = tx.send(val);
                    return;
                }
            }
        }
    });
    let reply = sent
        .map_err(|e| Error::new(2, format!("claude probe stdin: {e}")))
        .and_then(|_| {
            rx.recv_timeout(Duration::from_secs(20))
                .map_err(|_| Error::new(2, "claude probe timed out"))
        });
    let _ = child.kill();
    let _ = child.wait();

    let reply = reply?;
    if claude_probe_says_no_auth(&reply) {
        return Err(Error::new(2, NOT_LOGGED_IN));
    }

    let gauges = parse_claude_usage(&reply["response"]["response"]);
    if gauges.is_empty() {
        return Err(Error::new(2, "claude probe returned no rate limits"));
    }
    Ok(gauges)
}

/// Windows Claude reports, with the model-specific weekly ones only when the
/// plan has them.
fn parse_claude_usage(usage: &Value) -> Vec<Gauge> {
    const WEEK: u64 = 7 * 24 * 3600;
    const WINDOWS: [(&str, &str, &str, u64); 4] = [
        ("five_hour", "claude:session", "5h", 5 * 3600),
        ("seven_day", "claude:weekly", "week", WEEK),
        ("seven_day_opus", "claude:weekly_opus", "opus week", WEEK),
        (
            "seven_day_sonnet",
            "claude:weekly_sonnet",
            "sonnet week",
            WEEK,
        ),
    ];
    WINDOWS
        .iter()
        .filter_map(|(key, id, label, secs)| {
            let window = &usage["rate_limits"][key];
            Some(gauge(
                id,
                *label,
                percent(window["utilization"].as_f64()?),
                window["resets_at"].as_str().map(str::to_string),
                Some(*secs),
            ))
        })
        .collect()
}

fn fetch_claude() -> ProviderView {
    let (windows, error) = match run_claude_usage_probe() {
        Ok(windows) => (windows, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    provider_view("claude", "claude", "✳", windows, error)
}

/// Plan and quota bars. Never log credential values.
pub fn print_usage() -> Result<i32, Error> {
    type Fetch = fn() -> ProviderView;
    let providers: [(&str, Fetch); 4] = [
        ("claude", fetch_claude),
        ("agy", fetch_gemini),
        ("codex", fetch_codex),
        ("grok", fetch_grok),
    ];
    let mut views: Vec<ProviderView> = providers
        .iter()
        .filter(|(bin, _)| crate::providers::binary_on_path(bin))
        .map(|(_, fetch)| fetch())
        .collect();

    for view in views
        .iter_mut()
        .filter(|v| v.error.as_deref() != Some(NOT_LOGGED_IN))
    {
        let signal = match view.key.as_str() {
            "claude" => plan::detect_claude(),
            "grok" => plan::detect_grok(),
            "codex" => plan::detect_codex(),
            _ => plan::PlanSignal::Unknown,
        };
        view.plan = plan::show(&view.key, signal);
    }

    print!("{}", render_blocks(&views, std::io::stdout().is_terminal()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_rate_limit_labels_window_by_length() {
        let payload = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 604800,
                    "reset_at": 1793090890
                },
                "secondary_window": null
            }
        });
        let gauges = parse_codex_rate_limit(&payload);
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].id, "codex:primary_window");
        assert_eq!(gauges[0].label, "Codex · week");
        assert_eq!(gauges[0].percent, 100);
        assert_eq!(gauges[0].window_secs, Some(604800));
    }

    #[test]
    fn claude_usage_skips_absent_windows() {
        let usage = serde_json::json!({
            "rate_limits": {
                "five_hour": { "utilization": 18, "resets_at": "2026-10-22T23:49:59+00:00" },
                "seven_day": { "utilization": 53.6, "resets_at": "2026-10-27T11:59:59+00:00" },
                "seven_day_opus": null
            }
        });
        let gauges = parse_claude_usage(&usage);
        assert_eq!(gauges.len(), 2);
        assert_eq!((gauges[1].label.as_str(), gauges[1].percent), ("week", 54));
        assert!(
            gauges[1]
                .reset
                .as_deref()
                .and_then(render::reset_instant)
                .is_some()
        );
    }

    #[test]
    fn antigravity_buckets_become_used_percent() {
        let root = serde_json::json!({ "command": { "data": { "groups": [{
            "name": "Gemini Models",
            "buckets": [{
                "id": "gemini-5h",
                "window": "5h",
                "remaining_fraction": 0.72,
                "reset_time": "2026-10-24T17:29:30Z"
            }]
        }] } } });
        let gauges = parse_antigravity(&root);
        assert_eq!(gauges.len(), 1);
        assert_eq!(
            (gauges[0].label.as_str(), gauges[0].percent),
            ("gemini 5h", 28)
        );
        assert_eq!(gauges[0].window_secs, Some(5 * 3600));
    }

    #[test]
    fn pty_probe_kills_grandchild_that_ignores_hangup() {
        let dir = std::env::temp_dir().join(format!("crew-pty-group-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let pid_path = dir.join("pid");
        let script = r#"
import os, signal, sys, time
path = sys.argv[1]
if os.fork() == 0:
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    with open(path, "w") as handle:
        handle.write(str(os.getpid()))
    time.sleep(60)
else:
    while not (os.path.exists(path) and os.path.getsize(path) > 0):
        time.sleep(0.02)
    os._exit(0)
"#;
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(script).arg(&pid_path);
        run_on_pty(cmd, Duration::from_secs(5)).expect("pty probe");
        let pid = fs::read_to_string(&pid_path).unwrap().trim().to_string();
        let _ = fs::remove_dir_all(&dir);

        let running = || {
            let listed = Command::new("ps")
                .args(["-o", "state=", "-p", &pid])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&listed.stdout).trim().to_string();
            state.starts_with(['S', 'R', 'U', 'I'])
        };
        for _ in 0..50 {
            if !running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(), "grandchild {pid} still running");
    }

    #[test]
    fn missing_or_blank_credentials_mean_not_logged_in() {
        let home = std::env::temp_dir().join(format!("crew-auth-test-{}", std::process::id()));
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".grok")).unwrap();
        assert_eq!(grok_token(&home), Err(NOT_LOGGED_IN));
        fs::write(
            home.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":""}}"#,
        )
        .unwrap();
        assert_eq!(codex_token(&home), Err(NOT_LOGGED_IN));
        fs::write(
            home.join(".grok/auth.json"),
            r#"{"default":{"auth_mode":"api_key"}}"#,
        )
        .unwrap();
        assert_eq!(grok_token(&home), Err(NOT_LOGGED_IN));
        let _ = fs::remove_dir_all(&home);

        assert!(is_agy_unauthenticated(
            "You are not logged into Antigravity"
        ));
        assert!(!is_agy_unauthenticated(r#"{"groups": []}"#));
        let auth_error = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "error", "error": "User is not authenticated." }
        });
        assert!(claude_probe_says_no_auth(&auth_error));
        assert!(claude_probe_says_no_auth(
            &serde_json::json!({ "loggedIn": false })
        ));
        assert!(!claude_probe_says_no_auth(
            &serde_json::json!({ "type": "control_response" })
        ));
    }
}
