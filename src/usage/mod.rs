#![allow(clippy::collapsible_if)]

pub mod render;

use std::fs;
use std::io::IsTerminal;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

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
    // Base64url padding
    let mut padded = payload.replace('-', "+").replace('_', "/");
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }

    // Decode without external base64 crate
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

fn fetch_codex(last_ok: Option<&Value>) -> ProviderView {
    let home = std::env::var("HOME").unwrap_or_default();
    let auth_path = Path::new(&home).join(".codex/auth.json");

    let mut token: Option<String> = None;
    let mut plan: Option<String> = None;

    if auth_path.is_file() {
        if let Ok(content) = fs::read_to_string(&auth_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                if let Some(t) = val["tokens"]["access_token"].as_str() {
                    token = Some(t.to_string());
                    plan = decode_jwt_plan(t);
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    if let Some(ref tok) = token {
        let output = Command::new("curl")
            .arg("-s")
            .arg("-m")
            .arg("10")
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", tok))
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("https://chatgpt.com/backend-api/wham/usage")
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                if let Ok(resp) = serde_json::from_slice::<Value>(&out.stdout) {
                    if let Some(rate_limit) = resp.get("rate_limit") {
                        if let Some(pct) = rate_limit["used_percent"].as_f64() {
                            windows.push(Gauge {
                                id: "codex:primary_window".to_string(),
                                label: "Codex · week".to_string(),
                                percent: pct.round() as u32,
                                reset: rate_limit["resets_at"].as_str().map(|s| s.to_string()),
                                stale: false,
                            });
                        }
                    }
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
        } else if token.is_none() {
            error = Some("not signed in".to_string());
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

fn fetch_grok(last_ok: Option<&Value>) -> ProviderView {
    let home = std::env::var("HOME").unwrap_or_default();
    let auth_path = Path::new(&home).join(".grok/auth.json");

    let mut token: Option<String> = None;
    if auth_path.is_file() {
        if let Ok(content) = fs::read_to_string(&auth_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                if let Some(k) = val["key"].as_str() {
                    token = Some(k.to_string());
                } else if let Some(t) = val["tokens"]["access_token"].as_str() {
                    token = Some(t.to_string());
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    if let Some(ref tok) = token {
        let output = Command::new("curl")
            .arg("-s")
            .arg("-m")
            .arg("10")
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", tok))
            .arg("https://cli-chat-proxy.grok.com/v1/billing?format=credits")
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                if let Ok(resp) = serde_json::from_slice::<Value>(&out.stdout) {
                    if let Some(pct) = resp["percent_used"].as_f64() {
                        windows.push(Gauge {
                            id: "grok:weekly".to_string(),
                            label: "Grok · week".to_string(),
                            percent: pct.round() as u32,
                            reset: resp["resets_at"].as_str().map(|s| s.to_string()),
                            stale: false,
                        });
                    }
                }
            }
        }
    }

    if windows.is_empty() {
        if let Some(saved) = last_ok.and_then(|v| v.get("grok")) {
            windows = parse_gauges_from_json(Some(saved));
            for w in &mut windows {
                w.stale = true;
            }
        } else if token.is_none() {
            error = Some("not signed in".to_string());
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
        // Pre-check TCP connectivity
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
            // Run agy with PTY or command
            let output = Command::new("agy")
                .arg("-p")
                .arg("/usage")
                .arg("--output-format")
                .arg("json")
                .output();

            if let Ok(out) = output {
                if out.status.success() {
                    if let Ok(parsed) = serde_json::from_slice::<Value>(&out.stdout) {
                        let _ = fs::write(&cache_file, &out.stdout);
                        raw_json = Some(parsed);
                    }
                }
            }
        }
    }

    let mut windows = Vec::new();
    let mut error = None;

    if let Some(ref val) = raw_json {
        if let Some(arr) = val.as_array().or_else(|| val["models"].as_array()) {
            for item in arr {
                let name = item["name"].as_str().unwrap_or("Model");
                let window = item["window"].as_str().unwrap_or("week");
                let percent = item["percent"]
                    .as_f64()
                    .or_else(|| {
                        item["remaining_percent"]
                            .as_f64()
                            .map(|rem| (100.0 - rem).max(0.0))
                    })
                    .unwrap_or(0.0);

                let id = format!("antigravity:{}-{}", name.to_lowercase(), window);
                let label = format!("{} · {}", name, window);
                windows.push(Gauge {
                    id,
                    label,
                    percent: percent.round() as u32,
                    reset: item["reset"].as_str().map(|s| s.to_string()),
                    stale: false,
                });
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

    if let Some(saved) = last_ok.and_then(|v| v.get("claude")) {
        windows = parse_gauges_from_json(Some(saved));
    } else {
        error = Some("no recent claude session".to_string());
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
