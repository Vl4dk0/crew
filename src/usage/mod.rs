#![allow(clippy::collapsible_if)]

mod plan;
pub mod render;

use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::providers::{detach_session, terminate_group};

use serde_json::Value;

use crate::error::Error;
use render::{Gauge, ProviderView, render_blocks};

fn state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("USAGEBAR_STATE") {
        PathBuf::from(p)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Library/Application Support/UsageBar")
    } else {
        PathBuf::from(".")
    }
}

/// The probe's descendants stay in its session. Drop signals that group so
/// MCP servers do not keep running after `agy` has already exited.
struct ProbeChild {
    child: Child,
}

impl ProbeChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        terminate_group(&mut self.child);
    }
}

fn run_on_pty(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, Error> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    // agy starts MCP servers as grandchildren. Own the session so the probe
    // can signal them on the way out, not only the direct child.
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

    let mut child = ProbeChild {
        child: cmd
            .spawn()
            .map_err(|e| Error::new(2, format!("failed to spawn on pty: {}", e)))?,
    };

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

    Ok(output)
}

pub(crate) fn parse_codex_rate_limit(resp: &Value) -> Vec<Gauge> {
    let mut windows = Vec::new();
    if let Some(rate_limit) = resp.get("rate_limit").and_then(|r| r.as_object()) {
        for key in ["primary_window", "secondary_window"] {
            if let Some(win) = rate_limit.get(key).and_then(|w| w.as_object()) {
                if let Some(pct) = win.get("used_percent").and_then(|u| u.as_f64()) {
                    let seconds = win
                        .get("limit_window_seconds")
                        .and_then(|s| s.as_i64())
                        .unwrap_or(0);
                    let window_label = if seconds > 0 && seconds < 48 * 3600 {
                        format!("{}h", (seconds as f64 / 3600.0).round() as i64)
                    } else {
                        "week".to_string()
                    };
                    let reset = win.get("reset_at").and_then(|r| {
                        if let Some(epoch) = r.as_i64() {
                            Some(epoch.to_string())
                        } else {
                            r.as_str().map(|s| s.to_string())
                        }
                    });
                    let window_secs = if seconds > 0 {
                        Some(seconds as u64)
                    } else {
                        None
                    };
                    windows.push(Gauge {
                        id: format!("codex:{}", key),
                        label: format!("Codex · {}", window_label),
                        percent: pct.round().clamp(0.0, 100.0) as u32,
                        reset,
                        window_secs,
                    });
                }
            }
        }
    }
    windows
}

/// Read Codex credentials from ~/.codex/auth.json. Missing or empty token indicates not logged in.
fn codex_token(home: &Path) -> Result<(String, Option<String>), &'static str> {
    let content = fs::read_to_string(home.join(".codex/auth.json")).map_err(|_| "not logged in")?;
    let val: Value = serde_json::from_str(&content).map_err(|_| "not logged in")?;
    let token = val["tokens"]["access_token"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .ok_or("not logged in")?
        .to_string();
    let account_id = val["tokens"]["account_id"]
        .as_str()
        .filter(|a| !a.trim().is_empty())
        .map(str::to_string);
    Ok((token, account_id))
}

fn fetch_codex() -> ProviderView {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (tok, account_id) = match codex_token(&home) {
        Ok(creds) => creds,
        Err(err) => {
            return ProviderView {
                key: "codex".to_string(),
                name: "codex".to_string(),
                mark: "⚛".to_string(),
                plan: None,
                windows: Vec::new(),
                error: Some(err.to_string()),
            };
        }
    };

    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-m", "10", "-w", "\n%{http_code}"])
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", tok))
        .arg("-H")
        .arg("User-Agent: UsageBar")
        .arg("-H")
        .arg("Content-Type: application/json");
    if let Some(ref acc) = account_id {
        cmd.arg("-H").arg(format!("ChatGPT-Account-Id: {}", acc));
    }
    cmd.arg("https://chatgpt.com/backend-api/wham/usage");

    let mut windows = Vec::new();
    let mut error = None;

    if let Ok(out) = cmd.output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.trim().lines().collect();
        let (body, code) = if lines.len() >= 2 {
            let code: u16 = lines.last().unwrap().parse().unwrap_or(0);
            let body = lines[..lines.len() - 1].join("\n");
            (body, code)
        } else {
            (
                text.into_owned(),
                if out.status.success() { 200 } else { 0 },
            )
        };

        if code == 401 || code == 403 {
            error = Some("not logged in".to_string());
        } else if let Ok(resp) = serde_json::from_str::<Value>(&body) {
            windows = parse_codex_rate_limit(&resp);
        }
    }

    if windows.is_empty() && error.is_none() {
        error = Some("failed to load usage".to_string());
    }

    ProviderView {
        key: "codex".to_string(),
        name: "codex".to_string(),
        mark: "⚛".to_string(),
        plan: None,
        windows,
        error,
    }
}

/// Check if agy probe output indicates the user is unauthenticated.
pub(crate) fn is_agy_unauthenticated(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not authenticated")
        || lower.contains("you are not logged into antigravity")
        || lower.contains("unauthenticated")
}

fn fetch_gemini() -> ProviderView {
    let cache_file = state_dir().join("antigravity_cache.json");
    let mut raw_json: Option<Value> = None;
    let mut error = None;

    if cache_file.is_file() {
        if let Ok(meta) = fs::metadata(&cache_file) {
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = SystemTime::now().duration_since(modified) {
                    if elapsed < Duration::from_secs(120) {
                        if let Ok(txt) = fs::read_to_string(&cache_file) {
                            raw_json = serde_json::from_str(&txt).ok();
                        }
                    }
                }
            }
        }
    }

    if raw_json.is_none() {
        let addr = "oauth2.googleapis.com:443"
            .parse::<SocketAddr>()
            .or_else(|_| {
                use std::net::ToSocketAddrs;
                "oauth2.googleapis.com:443"
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut iter| iter.next())
                    .ok_or(())
            });

        let can_connect = match addr {
            Ok(sa) => TcpStream::connect_timeout(&sa, Duration::from_secs(3)).is_ok(),
            Err(_) => false,
        };

        if can_connect {
            let mut cmd = Command::new("agy");
            cmd.arg("-p")
                .arg("/usage")
                .arg("--output-format")
                .arg("json");

            if let Ok(output) = run_on_pty(cmd, Duration::from_secs(25)) {
                let text = String::from_utf8_lossy(&output);
                if is_agy_unauthenticated(&text) {
                    error = Some("not logged in".to_string());
                } else if let Ok(parsed) = serde_json::from_slice::<Value>(&output) {
                    let _ = fs::write(&cache_file, &output);
                    raw_json = Some(parsed);
                }
            }
        }
    }

    let mut windows = Vec::new();

    if let Some(ref root) = raw_json {
        let groups = root
            .get("command")
            .and_then(|c| c.get("data"))
            .and_then(|d| d.get("groups"))
            .or_else(|| root.get("data").and_then(|d| d.get("groups")))
            .or_else(|| root.get("groups"))
            .and_then(|g| g.as_array());

        if let Some(arr) = groups {
            for group in arr {
                let gname = group["displayName"]
                    .as_str()
                    .or_else(|| group["name"].as_str())
                    .unwrap_or("");

                if let Some(buckets) = group["buckets"].as_array() {
                    for bucket in buckets {
                        let bid = bucket["bucketId"]
                            .as_str()
                            .or_else(|| bucket["id"].as_str())
                            .unwrap_or("");
                        let rem = bucket["remainingFraction"]
                            .as_f64()
                            .or_else(|| bucket["remaining_fraction"].as_f64())
                            .unwrap_or(1.0);
                        let window_type = bucket["window"].as_str().unwrap_or("");
                        let reset = bucket["resetTime"]
                            .as_str()
                            .or_else(|| bucket["reset_time"].as_str())
                            .map(|s| s.to_string());

                        let percent = ((1.0 - rem) * 100.0).round().clamp(0.0, 100.0) as u32;

                        let label = if gname.contains("Gemini Models") || gname == "Gemini" {
                            if window_type == "5h" {
                                "gemini 5h".to_string()
                            } else {
                                "gemini week".to_string()
                            }
                        } else if gname.contains("Claude") || gname.contains("Other") {
                            if window_type == "5h" {
                                "other models 5h".to_string()
                            } else {
                                "other models week".to_string()
                            }
                        } else if gname.contains("Argon") {
                            "Gemini 4 Argon week".to_string()
                        } else {
                            format!("{} {}", gname, window_type)
                        };

                        let window_secs = match window_type {
                            "5h" => Some(5 * 3600),
                            "weekly" => Some(7 * 24 * 3600),
                            _ => None,
                        };
                        windows.push(Gauge {
                            id: format!("antigravity:{}", bid),
                            label,
                            percent,
                            reset,
                            window_secs,
                        });
                    }
                }
            }
        }
    }

    if windows.is_empty() && error.is_none() {
        error = Some("failed to load usage".to_string());
    }

    ProviderView {
        key: "gemini".to_string(),
        name: "antigravity".to_string(),
        mark: "✦".to_string(),
        plan: None,
        windows,
        error,
    }
}

/// Read Grok OIDC credentials from ~/.grok/auth.json. Missing or expired token indicates not logged in.
fn grok_token(home: &Path) -> Result<(String, Option<String>), &'static str> {
    let content = fs::read_to_string(home.join(".grok/auth.json")).map_err(|_| "not logged in")?;
    let data: Value = serde_json::from_str(&content).map_err(|_| "not logged in")?;
    let obj = data.as_object().ok_or("not logged in")?;

    let mut best: Option<(String, String)> = None;
    for entry in obj.values() {
        if entry.get("auth_mode").and_then(|m| m.as_str()) == Some("oidc") {
            if let Some(key) = entry.get("key").and_then(|k| k.as_str()) {
                if !key.trim().is_empty() {
                    let exp = entry
                        .get("expires_at")
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string();
                    if best.is_none() || exp > best.as_ref().unwrap().1 {
                        best = Some((key.to_string(), exp));
                    }
                }
            }
        }
    }

    let (k, e) = best.ok_or("not logged in")?;
    let exp_opt = if e.is_empty() { None } else { Some(e) };
    Ok((k, exp_opt))
}

fn fetch_grok() -> ProviderView {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut token_info = match grok_token(&home) {
        Ok(creds) => Some(creds),
        Err(err) => {
            return ProviderView {
                key: "grok".to_string(),
                name: "grok".to_string(),
                mark: "✕".to_string(),
                plan: None,
                windows: Vec::new(),
                error: Some(err.to_string()),
            };
        }
    };

    let refresh_grok = || {
        let _ = Command::new("grok")
            .arg("models")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    };

    // Check expiry (< 120s)
    if let Some((_, Some(ref exp_str))) = token_info {
        if let Some(exp_time) = crate::job::parse_iso_time(exp_str) {
            if let Ok(remaining) = exp_time.duration_since(SystemTime::now()) {
                if remaining < Duration::from_secs(120) {
                    refresh_grok();
                    token_info = grok_token(&home).ok();
                }
            } else {
                // Already expired
                refresh_grok();
                token_info = grok_token(&home).ok();
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    if let Some((tok, _)) = token_info {
        let query_api = |token: &str, query: &str| -> Result<(Value, u16), ()> {
            let url = format!("https://cli-chat-proxy.grok.com/v1/billing{}", query);
            let out = Command::new("curl")
                .arg("-s")
                .arg("-m")
                .arg("10")
                .arg("-w")
                .arg("\n%{http_code}")
                .arg("-H")
                .arg(format!("Authorization: Bearer {}", token))
                .arg("-H")
                .arg("X-XAI-Token-Auth: xai-grok-cli")
                .arg(&url)
                .output();

            if let Ok(res) = out {
                let text = String::from_utf8_lossy(&res.stdout);
                let lines: Vec<&str> = text.trim().lines().collect();
                if lines.len() >= 2 {
                    let code: u16 = lines.last().unwrap().parse().unwrap_or(0);
                    let body = lines[..lines.len() - 1].join("\n");
                    if let Ok(val) = serde_json::from_str::<Value>(&body) {
                        return Ok((val, code));
                    }
                }
            }
            Err(())
        };

        let mut weekly_res = query_api(&tok, "?format=credits");
        let effective_tok = if let Ok((_, code)) = weekly_res {
            if code == 401 || code == 403 {
                refresh_grok();
                grok_token(&home).map(|(t, _)| t).unwrap_or(tok.clone())
            } else {
                tok.clone()
            }
        } else {
            tok.clone()
        };

        if effective_tok != tok {
            weekly_res = query_api(&effective_tok, "?format=credits");
        }

        if let Ok((_, code)) = weekly_res {
            if code == 401 || code == 403 {
                error = Some("not logged in".to_string());
            }
        }

        let monthly_res = if error.is_none() {
            query_api(&effective_tok, "")
        } else {
            Err(())
        };

        let mut weekly_cfg: Option<Value> = None;
        if let Ok((weekly_val, _)) = weekly_res {
            let cfg = weekly_val.get("config").unwrap_or(&weekly_val).clone();
            let mut credit_pct = cfg["creditUsagePercent"].as_f64();

            if credit_pct.is_none() {
                if let Some(products) = cfg["productUsage"].as_array() {
                    for item in products {
                        let name = item["product"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase()
                            .replace(['-', '_'], "");
                        if name == "grokbuild" || name == "build" {
                            credit_pct = item["usagePercent"].as_f64();
                            break;
                        }
                    }
                }
            }

            if credit_pct.is_none() {
                if let Some(ptype) = cfg["currentPeriod"]["type"].as_str() {
                    if ptype.to_uppercase().contains("WEEK") {
                        credit_pct = Some(0.0);
                    }
                }
            }

            if let Some(pct) = credit_pct {
                let reset = cfg["currentPeriod"]["end"]
                    .as_str()
                    .or_else(|| cfg["billingPeriodEnd"].as_str())
                    .map(|s| s.to_string());
                let weekly_type = cfg["currentPeriod"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.to_uppercase().contains("WEEK"));
                let window_secs = period_window_secs(
                    cfg["currentPeriod"]["start"].as_str(),
                    cfg["currentPeriod"]["end"].as_str(),
                    weekly_type && reset.is_some(),
                );

                windows.push(Gauge {
                    id: "grok:weekly".to_string(),
                    label: "week".to_string(),
                    percent: pct.round().clamp(0.0, 100.0) as u32,
                    reset,
                    window_secs,
                });
            }

            weekly_cfg = Some(cfg);
        }

        let mut monthly_cfg: Option<Value> = None;
        if let Ok((monthly_val, code)) = monthly_res {
            if code == 200 {
                let mcfg = monthly_val.get("config").unwrap_or(&monthly_val).clone();
                let limit = mcfg["monthlyLimit"]["val"]
                    .as_f64()
                    .or_else(|| mcfg["monthlyLimit"].as_f64());
                let used = mcfg["used"]["val"]
                    .as_f64()
                    .or_else(|| mcfg["used"].as_f64());
                if let (Some(lim), Some(u)) = (limit, used) {
                    if lim > 0.0 {
                        let pct = (u / lim * 100.0).round().clamp(0.0, 100.0) as u32;
                        let reset = mcfg["billingPeriodEnd"].as_str().map(|s| s.to_string());
                        let window_secs = period_window_secs(
                            mcfg["currentPeriod"]["start"].as_str(),
                            mcfg["currentPeriod"]["end"].as_str(),
                            false,
                        );
                        windows.push(Gauge {
                            id: "grok:monthly".to_string(),
                            label: "month".to_string(),
                            percent: pct,
                            reset,
                            window_secs,
                        });
                    }
                }
                monthly_cfg = Some(mcfg);
            }
        }

        // Check onDemandCap from weekly, falling back to monthly if missing on weekly
        let (ondemand_cap, ondemand_spent, ondemand_reset) = {
            let weekly_cap = weekly_cfg.as_ref().and_then(|c| {
                c["onDemandCap"]["val"]
                    .as_f64()
                    .or_else(|| c["onDemandCap"].as_f64())
            });
            let weekly_spent = weekly_cfg.as_ref().and_then(|c| {
                c["onDemandUsed"]["val"]
                    .as_f64()
                    .or_else(|| c["onDemandUsed"].as_f64())
            });

            if weekly_cap.is_some() {
                let reset = weekly_cfg.as_ref().and_then(|c| {
                    c["billingPeriodEnd"]
                        .as_str()
                        .or_else(|| c["currentPeriod"]["end"].as_str())
                        .map(|s| s.to_string())
                });
                (weekly_cap, weekly_spent, reset)
            } else {
                let monthly_cap = monthly_cfg.as_ref().and_then(|c| {
                    c["onDemandCap"]["val"]
                        .as_f64()
                        .or_else(|| c["onDemandCap"].as_f64())
                });
                let monthly_spent = monthly_cfg.as_ref().and_then(|c| {
                    c["onDemandUsed"]["val"]
                        .as_f64()
                        .or_else(|| c["onDemandUsed"].as_f64())
                });
                let reset = monthly_cfg.as_ref().and_then(|c| {
                    c["billingPeriodEnd"]
                        .as_str()
                        .or_else(|| c["currentPeriod"]["end"].as_str())
                        .map(|s| s.to_string())
                });
                (monthly_cap, monthly_spent, reset)
            }
        };

        if let (Some(cap), Some(spent)) = (ondemand_cap, ondemand_spent) {
            if cap > 0.0 {
                let pct = (spent / cap * 100.0).round().clamp(0.0, 100.0) as u32;
                windows.push(Gauge {
                    id: "grok:ondemand".to_string(),
                    label: "pay-as-you-go".to_string(),
                    percent: pct,
                    reset: ondemand_reset,
                    window_secs: None,
                });
            }
        }
    }

    if windows.is_empty() && error.is_none() {
        error = Some("failed to load usage".to_string());
    }

    ProviderView {
        key: "grok".to_string(),
        name: "grok".to_string(),
        mark: "✕".to_string(),
        plan: None,
        windows,
        error,
    }
}

/// Measured start/end when both parse. A weekly period type is 7 days only
/// when the payload says so and an end exists; a missing start is not guessed.
fn period_window_secs(start: Option<&str>, end: Option<&str>, weekly: bool) -> Option<u64> {
    if let (Some(start), Some(end)) = (start, end) {
        if let (Some(start), Some(end)) = (render::reset_instant(start), render::reset_instant(end))
        {
            if let Ok(span) = end.duration_since(start) {
                if span.as_secs() > 0 {
                    return Some(span.as_secs());
                }
            }
        }
    }
    if weekly { Some(7 * 24 * 3600) } else { None }
}

/// Check if Claude stream-json output indicates the user is unauthenticated.
pub(crate) fn claude_probe_says_no_auth(val: &Value) -> bool {
    if val["type"] == "control_response" {
        let resp = &val["response"];
        if resp["subtype"] == "error" {
            let msg = resp["error"].as_str().unwrap_or("");
            let m = msg.to_ascii_lowercase();
            return m.is_empty()
                || m.contains("auth")
                || m.contains("login")
                || m.contains("logged in")
                || m.contains("unauthorized")
                || m.contains("token")
                || m.contains("credential");
        }
    }
    if val.get("loggedIn") == Some(&Value::Bool(false)) {
        return true;
    }
    false
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
                if val["type"] == "control_response" && val["response"]["request_id"] == "usage" {
                    let _ = tx.send(val);
                    return;
                }
                if claude_probe_says_no_auth(&val) {
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
        return Err(Error::new(2, "not logged in"));
    }

    let gauges = parse_claude_usage(&reply["response"]["response"]);
    if gauges.is_empty() {
        Err(Error::new(2, "claude probe returned no rate limits"))
    } else {
        Ok(gauges)
    }
}

/// Windows Claude reports, with the model-specific weekly ones only when the
/// plan has them.
fn parse_claude_usage(usage: &Value) -> Vec<Gauge> {
    const WINDOWS: [(&str, &str, &str, u64); 4] = [
        ("five_hour", "claude:session", "5h", 5 * 3600),
        ("seven_day", "claude:weekly", "week", 7 * 24 * 3600),
        (
            "seven_day_opus",
            "claude:weekly_opus",
            "opus week",
            7 * 24 * 3600,
        ),
        (
            "seven_day_sonnet",
            "claude:weekly_sonnet",
            "sonnet week",
            7 * 24 * 3600,
        ),
    ];
    WINDOWS
        .iter()
        .filter_map(|(key, id, label, secs)| {
            let window = &usage["rate_limits"][key];
            Some(Gauge {
                id: id.to_string(),
                label: label.to_string(),
                percent: window["utilization"].as_f64()?.round().clamp(0.0, 100.0) as u32,
                reset: window["resets_at"].as_str().map(str::to_string),
                window_secs: Some(*secs),
            })
        })
        .collect()
}

fn fetch_claude() -> ProviderView {
    let (windows, error) = match run_claude_usage_probe() {
        Ok(windows) => (windows, None),
        Err(e) => {
            let msg = if e.message == "not logged in" {
                "not logged in".to_string()
            } else {
                e.to_string()
            };
            (Vec::new(), Some(msg))
        }
    };

    ProviderView {
        key: "claude".to_string(),
        name: "claude".to_string(),
        mark: "✳".to_string(),
        plan: None,
        windows,
        error,
    }
}

/// Plan and quota bars. Never log credential values.
pub fn print_usage() -> Result<i32, Error> {
    let mut views = Vec::new();
    if crate::providers::binary_on_path("claude") {
        views.push(fetch_claude());
    }
    if crate::providers::binary_on_path("agy") {
        views.push(fetch_gemini());
    }
    if crate::providers::binary_on_path("codex") {
        views.push(fetch_codex());
    }
    if crate::providers::binary_on_path("grok") {
        views.push(fetch_grok());
    }

    for view in &mut views {
        if view.error.as_deref() == Some("not logged in") {
            continue;
        }
        let signal = match view.key.as_str() {
            "claude" => plan::detect_claude(),
            "grok" => plan::detect_grok(),
            "codex" => plan::detect_codex(),
            _ => plan::PlanSignal::Unknown,
        };
        view.plan = plan::show(&view.key, signal);
    }

    let tty = std::io::stdout().is_terminal();
    let rendered = render_blocks(&views, tty);
    print!("{rendered}");

    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_ondemand_cap_fallback_to_monthly() {
        let weekly = serde_json::json!({
            "creditUsagePercent": 15.0,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "end": "2026-10-23T06:26:08Z"
            }
        });
        let monthly = serde_json::json!({
            "monthlyLimit": { "val": 100.0 },
            "used": { "val": 20.0 },
            "onDemandCap": { "val": 50.0 },
            "onDemandUsed": { "val": 10.0 },
            "billingPeriodEnd": "2026-10-31T00:00:00Z"
        });

        let weekly_cap = weekly.get("onDemandCap").and_then(|c| c["val"].as_f64());
        let ondemand_cap = if weekly_cap.is_some() {
            weekly_cap
        } else {
            monthly.get("onDemandCap").and_then(|c| c["val"].as_f64())
        };
        assert_eq!(ondemand_cap, Some(50.0));
    }

    #[test]
    fn test_parse_codex_rate_limit() {
        let payload = serde_json::json!({
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 561970,
                    "reset_at": 1793090890
                },
                "secondary_window": null
            }
        });
        let gauges = super::parse_codex_rate_limit(&payload);
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].id, "codex:primary_window");
        assert_eq!(gauges[0].label, "Codex · week");
        assert_eq!(gauges[0].percent, 100);
        assert_eq!(gauges[0].window_secs, Some(604800));
    }

    #[test]
    fn claude_usage_gives_exact_windows_and_skips_absent_ones() {
        let usage = serde_json::json!({
            "rate_limits": {
                "five_hour": { "utilization": 18, "resets_at": "2026-10-22T23:49:59.918732+00:00" },
                "seven_day": { "utilization": 53.6, "resets_at": "2026-10-27T11:59:59.918755+00:00" },
                "seven_day_opus": null
            }
        });
        let gauges = super::parse_claude_usage(&usage);
        assert_eq!(gauges.len(), 2);
        assert_eq!((gauges[1].label.as_str(), gauges[1].percent), ("week", 54));
        let reset = gauges[1]
            .reset
            .as_deref()
            .and_then(super::render::reset_instant);
        assert!(reset.is_some());
        assert_eq!(gauges[1].window_secs, Some(7 * 24 * 3600));
    }

    #[test]
    fn pty_probe_kills_grandchild_that_ignores_hangup() {
        let dir = std::env::temp_dir().join(format!("crew-pty-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_path = dir.join("pid");
        let script = r#"
import os, signal, sys, time
path = sys.argv[1]
pid = os.fork()
if pid == 0:
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    with open(path, "w") as handle:
        handle.write(str(os.getpid()))
        handle.flush()
    time.sleep(60)
else:
    for _ in range(100):
        if os.path.exists(path) and os.path.getsize(path) > 0:
            break
        time.sleep(0.02)
    os._exit(0)
"#;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c").arg(script).arg(&pid_path);
        super::run_on_pty(cmd, std::time::Duration::from_secs(5)).expect("pty probe");
        let pid: i32 = std::fs::read_to_string(&pid_path)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("pid integer");
        let mut state = String::new();
        for _ in 0..50 {
            let listed = std::process::Command::new("ps")
                .args(["-o", "state=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            state = String::from_utf8_lossy(&listed.stdout).trim().to_string();
            let running = state.starts_with('S')
                || state.starts_with('R')
                || state.starts_with('U')
                || state.starts_with('I');
            if !running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let running = state.starts_with('S')
            || state.starts_with('R')
            || state.starts_with('U')
            || state.starts_with('I');
        assert!(!running, "grandchild {pid} still running (state {state:?})");
    }

    #[test]
    fn test_unauthenticated_detection() {
        let temp = std::env::temp_dir().join(format!("crew-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        assert_eq!(super::codex_token(&temp), Err("not logged in"));
        let codex_dir = temp.join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("auth.json"),
            r#"{"tokens":{"access_token":""}}"#,
        )
        .unwrap();
        assert_eq!(super::codex_token(&temp), Err("not logged in"));

        assert_eq!(super::grok_token(&temp), Err("not logged in"));
        let grok_dir = temp.join(".grok");
        std::fs::create_dir_all(&grok_dir).unwrap();
        std::fs::write(
            grok_dir.join("auth.json"),
            r#"{"default":{"auth_mode":"api_key"}}"#,
        )
        .unwrap();
        assert_eq!(super::grok_token(&temp), Err("not logged in"));

        let _ = std::fs::remove_dir_all(&temp);

        assert!(super::is_agy_unauthenticated("not authenticated"));
        assert!(super::is_agy_unauthenticated(
            "You are not logged into Antigravity"
        ));
        assert!(super::is_agy_unauthenticated(
            r#"{"status": "UNAUTHENTICATED"}"#
        ));
        assert!(!super::is_agy_unauthenticated(r#"{"groups": []}"#));

        let claude_auth_err = serde_json::json!({
            "type": "control_response",
            "response": {
                "request_id": "usage",
                "subtype": "error",
                "error": "User is not authenticated. Please run claude login."
            }
        });
        assert!(super::claude_probe_says_no_auth(&claude_auth_err));
        assert!(super::claude_probe_says_no_auth(
            &serde_json::json!({ "loggedIn": false })
        ));
        let claude_ok = serde_json::json!({
            "type": "control_response",
            "response": { "request_id": "usage", "subtype": "success" }
        });
        assert!(!super::claude_probe_says_no_auth(&claude_ok));
    }
}
