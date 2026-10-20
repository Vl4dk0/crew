#![allow(clippy::collapsible_if)]

pub mod render;

use std::fs;
use std::io::{IsTerminal, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

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

fn last_ok_path() -> PathBuf {
    state_dir().join("last_ok.json")
}

fn load_last_ok() -> Option<Value> {
    let path = last_ok_path();
    if path.is_file() {
        if let Ok(content) = fs::read_to_string(&path) {
            return serde_json::from_str(&content).ok();
        }
    }
    None
}

fn parse_gauges_from_json(val: Option<&Value>) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    if let Some(Value::Array(arr)) = val {
        for item in arr {
            let id = item["id"].as_str().unwrap_or("").to_string();
            let label = item["label"].as_str().unwrap_or("").to_string();
            let percent = item["percent"].as_u64().unwrap_or(0) as u32;
            let reset = item["reset"].as_str().map(|s| s.to_string());
            let stale = item["stale"].as_bool().unwrap_or(false);
            if !label.is_empty() {
                gauges.push(Gauge {
                    id,
                    label,
                    percent,
                    reset,
                    stale,
                });
            }
        }
    }
    gauges
}

fn decode_jwt_plan(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload = parts[1];
    let mut padded = payload.replace('-', "+").replace('_', "/");
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }

    let bytes = simple_base64_decode(&padded)?;
    let json_val: Value = serde_json::from_slice(&bytes).ok()?;
    let plan = json_val["https://api.openai.com/auth"]["chatgpt_plan_type"].as_str()?;
    if plan.is_empty() {
        None
    } else {
        let mut chars = plan.chars();
        match chars.next() {
            None => None,
            Some(first) => {
                let capitalized = first.to_uppercase().collect::<String>() + chars.as_str();
                Some(capitalized)
            }
        }
    }
}

fn simple_base64_decode(input: &str) -> Option<Vec<u8>> {
    const B64_TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buffer = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;

    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let val = B64_TABLE.iter().position(|&x| x == b)? as u32;
        acc = (acc << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buffer.push((acc >> bits) as u8);
        }
    }
    Some(buffer)
}

fn run_on_pty(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, Error> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
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

    let _ = child.kill();
    let _ = child.wait();
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
                    windows.push(Gauge {
                        id: format!("codex:{}", key),
                        label: format!("Codex · {}", window_label),
                        percent: pct.round().clamp(0.0, 100.0) as u32,
                        reset,
                        stale: false,
                    });
                }
            }
        }
    }
    windows
}

fn fetch_codex(last_ok: Option<&Value>) -> ProviderView {
    let home = std::env::var("HOME").unwrap_or_default();
    let auth_path = Path::new(&home).join(".codex/auth.json");

    let mut token: Option<String> = None;
    let mut account_id: Option<String> = None;
    let mut plan: Option<String> = None;

    if auth_path.is_file() {
        if let Ok(content) = fs::read_to_string(&auth_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                if let Some(t) = val["tokens"]["access_token"].as_str() {
                    token = Some(t.to_string());
                    plan = decode_jwt_plan(t);
                }
                if let Some(acc) = val["tokens"]["account_id"].as_str() {
                    account_id = Some(acc.to_string());
                }
                if plan.is_none() {
                    if let Some(p) = val["account"]["plan_type"].as_str() {
                        let mut chars = p.chars();
                        if let Some(first) = chars.next() {
                            plan = Some(first.to_uppercase().collect::<String>() + chars.as_str());
                        }
                    }
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    if let Some(ref tok) = token {
        let mut cmd = Command::new("curl");
        cmd.arg("-s")
            .arg("-m")
            .arg("10")
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

        if let Ok(out) = cmd.output() {
            if out.status.success() {
                if let Ok(resp) = serde_json::from_slice::<Value>(&out.stdout) {
                    windows = parse_codex_rate_limit(&resp);
                }
            }
        }
    }

    if windows.is_empty() {
        if let Some(saved) = last_ok.and_then(|v| v.get("codex")) {
            windows = parse_gauges_from_json(Some(saved));
            for w in &mut windows {
                w.stale = true;
            }
        } else {
            error = Some("failed to load usage".to_string());
        }
    }

    ProviderView {
        key: "codex".to_string(),
        name: "codex".to_string(),
        mark: "⚛".to_string(),
        plan,
        windows,
        error,
    }
}

fn fetch_gemini(last_ok: Option<&Value>) -> ProviderView {
    let cache_file = state_dir().join("antigravity_cache.json");
    let mut raw_json: Option<Value> = None;

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
                if let Ok(parsed) = serde_json::from_slice::<Value>(&output) {
                    let _ = fs::write(&cache_file, &output);
                    raw_json = Some(parsed);
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

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

                        windows.push(Gauge {
                            id: format!("antigravity:{}", bid),
                            label,
                            percent,
                            reset,
                            stale: false,
                        });
                    }
                }
            }
        }
    }

    if windows.is_empty() {
        if let Some(saved) = last_ok.and_then(|v| v.get("gemini")) {
            windows = parse_gauges_from_json(Some(saved));
            for w in &mut windows {
                w.stale = true;
            }
        } else {
            error = Some("failed to load usage".to_string());
        }
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

fn fetch_grok(last_ok: Option<&Value>) -> ProviderView {
    let home = std::env::var("HOME").unwrap_or_default();
    let auth_path = Path::new(&home).join(".grok/auth.json");

    let extract_token = || -> Option<(String, Option<String>)> {
        let content = fs::read_to_string(&auth_path).ok()?;
        let data: Value = serde_json::from_str(&content).ok()?;
        let obj = data.as_object()?;

        let mut best: Option<(String, String)> = None;
        for entry in obj.values() {
            if entry.get("auth_mode").and_then(|m| m.as_str()) == Some("oidc") {
                if let Some(key) = entry.get("key").and_then(|k| k.as_str()) {
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

        best.map(|(k, e)| {
            let exp_opt = if e.is_empty() { None } else { Some(e) };
            (k, exp_opt)
        })
    };

    let refresh_grok = || {
        let _ = Command::new("grok")
            .arg("models")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    };

    let mut token_info = extract_token();

    // Check expiry (< 120s)
    if let Some((_, Some(ref exp_str))) = token_info {
        if let Some(exp_time) = crate::job::parse_iso_time(exp_str) {
            if let Ok(remaining) = exp_time.duration_since(SystemTime::now()) {
                if remaining < Duration::from_secs(120) {
                    refresh_grok();
                    token_info = extract_token();
                }
            } else {
                // Already expired
                refresh_grok();
                token_info = extract_token();
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
                extract_token()
                    .map(|(t, _)| t)
                    .unwrap_or_else(|| tok.clone())
            } else {
                tok.clone()
            }
        } else {
            tok.clone()
        };

        if effective_tok != tok {
            weekly_res = query_api(&effective_tok, "?format=credits");
        }

        let monthly_res = query_api(&effective_tok, "");

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

                windows.push(Gauge {
                    id: "grok:weekly".to_string(),
                    label: "week".to_string(),
                    percent: pct.round().clamp(0.0, 100.0) as u32,
                    reset,
                    stale: false,
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
                        windows.push(Gauge {
                            id: "grok:monthly".to_string(),
                            label: "month".to_string(),
                            percent: pct,
                            reset,
                            stale: false,
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
                    stale: false,
                });
            }
        }
    }

    if windows.is_empty() {
        if let Some(saved) = last_ok.and_then(|v| v.get("grok")) {
            windows = parse_gauges_from_json(Some(saved));
            for w in &mut windows {
                w.stale = true;
            }
        } else {
            error = Some("failed to load usage".to_string());
        }
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

fn strip_ansi(s: &str) -> String {
    let mut res = String::new();
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            res.push(c);
        }
    }
    res
}

fn run_claude_pty_probe() -> Result<Vec<Gauge>, Error> {
    let probe_dir = state_dir().join("ClaudeProbe");
    let _ = fs::create_dir_all(&probe_dir);

    use std::os::unix::io::{AsRawFd, FromRawFd};
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
        return Err(Error::new(2, "failed to openpty for claude probe"));
    }

    unsafe {
        let ws = libc::winsize {
            ws_row: 200,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(slave, libc::TIOCSWINSZ, &ws);
    }

    let slave_file = unsafe { fs::File::from_raw_fd(slave) };
    let slave_file_err = slave_file
        .try_clone()
        .map_err(|e| Error::new(2, format!("clone slave failed: {}", e)))?;

    let mut cmd = Command::new("claude");
    cmd.arg("--allowed-tools")
        .arg("")
        .arg("--strict-mcp-config")
        .arg("--settings")
        .arg("{\"disableAllHooks\":true,\"remoteControlAtStartup\":false}")
        .current_dir(&probe_dir)
        .env("TERM", "xterm-256color")
        .env("DISABLE_AUTOUPDATER", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_SESSION_ACCESS_TOKEN")
        .env_remove("CLAUDECODE");

    cmd.stdin(Stdio::from(slave_file.try_clone().unwrap()));
    cmd.stdout(Stdio::from(slave_file));
    cmd.stderr(Stdio::from(slave_file_err));

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::new(2, format!("failed to spawn claude probe: {}", e)))?;

    let mut master_file = unsafe { fs::File::from_raw_fd(master) };
    let master_fd = master_file.as_raw_fd();

    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL, 0);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let start = Instant::now();
    let timeout = Duration::from_secs(12);
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let mut sent_usage = false;

    while start.elapsed() < timeout {
        if !sent_usage && start.elapsed() > Duration::from_millis(1500) {
            sent_usage = true;
            let _ = unsafe { libc::write(master_fd, b"/usage\r".as_ptr() as *const _, 7) };
        }

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

        let text = String::from_utf8_lossy(&output);
        if text.contains("Current session") || text.contains("Current week") {
            std::thread::sleep(Duration::from_millis(500));
            while let Ok(n) = master_file.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buf[..n]);
            }
            break;
        }

        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    let text = strip_ansi(&String::from_utf8_lossy(&output));
    let mut gauges = Vec::new();

    // Parse session
    if let Some(pos) = text.find("Current session") {
        let snippet = &text[pos..pos + 200.min(text.len() - pos)];
        if let Some(pct) = extract_percent_from_text(snippet) {
            gauges.push(Gauge {
                id: "claude:session".to_string(),
                label: "5h".to_string(),
                percent: pct,
                reset: None,
                stale: false,
            });
        }
    }

    // Parse week
    if let Some(pos) = text.find("Current week") {
        let snippet = &text[pos..pos + 200.min(text.len() - pos)];
        if let Some(pct) = extract_percent_from_text(snippet) {
            gauges.push(Gauge {
                id: "claude:weekly".to_string(),
                label: "week".to_string(),
                percent: pct,
                reset: None,
                stale: false,
            });
        }
    }

    if gauges.is_empty() {
        Err(Error::new(2, "failed to parse claude probe output"))
    } else {
        Ok(gauges)
    }
}

fn extract_percent_from_text(s: &str) -> Option<u32> {
    for word in s.split_whitespace() {
        if let Some(idx) = word.find('%') {
            let num_part = &word[..idx];
            if let Ok(pct) = num_part.parse::<f64>() {
                if s.contains("left") || s.contains("remaining") {
                    return Some((100.0 - pct).round().clamp(0.0, 100.0) as u32);
                } else {
                    return Some(pct.round().clamp(0.0, 100.0) as u32);
                }
            }
        }
    }
    None
}

fn fetch_claude(last_ok: Option<&Value>) -> ProviderView {
    let mut plan = None;
    let retry_file = state_dir().join("claude_retry.json");
    if retry_file.is_file() {
        if let Ok(content) = fs::read_to_string(&retry_file) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                if let Some(p) = val["plan"].as_str() {
                    plan = Some(p.to_string());
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    // Check last_ok age
    let mut is_fresh = false;
    if let Some(saved) = last_ok.and_then(|v| v.get("claude")) {
        if let Some(arr) = saved.as_array() {
            if let Some(first) = arr.first() {
                if let Some(fetched_at) = first.get("fetched_at").and_then(|f| f.as_str()) {
                    if let Some(t) = crate::job::parse_iso_time(fetched_at) {
                        if let Ok(elapsed) = SystemTime::now().duration_since(t) {
                            if elapsed <= Duration::from_secs(180) {
                                is_fresh = true;
                            }
                        }
                    }
                }
            }
        }
        windows = parse_gauges_from_json(Some(saved));
    }

    if !is_fresh || windows.is_empty() {
        if let Ok(fresh) = run_claude_pty_probe() {
            windows = fresh;
        } else if !windows.is_empty() {
            for w in &mut windows {
                w.stale = true;
            }
        } else {
            error = Some("no recent claude session".to_string());
        }
    }

    ProviderView {
        key: "claude".to_string(),
        name: "claude".to_string(),
        mark: "✳".to_string(),
        plan,
        windows,
        error,
    }
}

/// Print the usagebar layout from the same sources. Never log credential values.
pub fn print_usage() -> Result<i32, Error> {
    let last_ok = load_last_ok();

    let views = vec![
        fetch_claude(last_ok.as_ref()),
        fetch_gemini(last_ok.as_ref()),
        fetch_codex(last_ok.as_ref()),
        fetch_grok(last_ok.as_ref()),
    ];

    let tty = std::io::stdout().is_terminal();
    let rendered = render_blocks(&views, tty);
    print!("{}", rendered);

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
        assert!(!gauges[0].stale);
    }
}
