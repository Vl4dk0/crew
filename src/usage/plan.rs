//! Plan name and monthly price shown by `crew usage`.
//!
//! Only what the local CLI state determines is shown: the name it reports, and
//! the public monthly list price when that exact plan has one. Nothing is read
//! from user config, so an undeterminable tier stays unpriced. An API-key login
//! is not presented as a subscription.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub(crate) enum PlanSignal {
    Named(String),
    /// Signed in with an API key, which is not a subscription.
    Api,
    Unknown,
}

pub(crate) struct ShownPlan {
    pub header: Option<String>,
    pub monthly_usd: Option<f64>,
}

/// Public monthly USD for a plan that has one list price.
///
/// Checked 2026-10-22. Codex "Pro" is several prices ($100, $200, $500) that
/// the login does not tell apart, and SuperGrok / Google AI tiers are not
/// detectable or not a single number, so they are absent.
fn list_price(provider: &str, name: &str) -> Option<f64> {
    match (provider, name.to_ascii_lowercase().as_str()) {
        ("claude", "pro") => Some(20.0),
        ("claude", "max 5x") => Some(100.0),
        ("claude", "max 20x") => Some(200.0),
        ("claude", "free") => Some(0.0),
        ("codex", "plus") => Some(20.0),
        ("codex", "go") => Some(8.0),
        ("codex", "free") => Some(0.0),
        _ => None,
    }
}

pub(crate) fn show(provider: &str, signal: PlanSignal) -> ShownPlan {
    let PlanSignal::Named(raw) = signal else {
        return ShownPlan {
            header: None,
            monthly_usd: None,
        };
    };
    let name = raw.trim();
    if name.is_empty() {
        return ShownPlan {
            header: None,
            monthly_usd: None,
        };
    }
    let price = list_price(provider, name);
    let header = match price {
        Some(usd) => format!("{name} {}/mo", format_usd(usd)),
        None => name.to_string(),
    };
    ShownPlan {
        header: Some(header),
        monthly_usd: price,
    }
}

pub(crate) fn format_usd(amount: f64) -> String {
    if !amount.is_finite() || amount < 0.0 {
        return "$0".to_string();
    }
    let cents = (amount * 100.0).round();
    let cents_i = cents as i64;
    if cents_i == 0 && amount > 0.0 {
        return format!("${amount:.4}");
    }
    if cents_i % 100 == 0 {
        return format!("${}", cents_i / 100);
    }
    format!("${:.2}", cents / 100.0)
}

pub(crate) fn detect_claude() -> PlanSignal {
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".claude.json");
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<ClaudeConfig>(&text) {
                if let Some(account) = cfg.oauth_account {
                    if let Some(name) = account
                        .organization_type
                        .as_deref()
                        .and_then(claude_plan_name)
                    {
                        let tier = (name == "Max").then(|| max_tier(&account)).flatten();
                        return PlanSignal::Named(tier.map_or(name, str::to_string));
                    }
                    if account.organization_type.is_none()
                        && billing_is_api(account.billing_type.as_deref())
                    {
                        return PlanSignal::Api;
                    }
                    if account.organization_type.is_some() {
                        return PlanSignal::Unknown;
                    }
                }
            }
        }
    }
    claude_auth_status()
}

fn billing_is_api(billing: Option<&str>) -> bool {
    let Some(billing) = billing else {
        return false;
    };
    let billing = billing.to_ascii_lowercase();
    billing.contains("api") && !billing.contains("subscription")
}

fn claude_auth_status() -> PlanSignal {
    let mut child = match std::process::Command::new("claude")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return PlanSignal::Unknown,
    };
    let started = std::time::Instant::now();
    let status = loop {
        if started.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return PlanSignal::Unknown;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return PlanSignal::Unknown,
        }
    };
    if !status.success() {
        return PlanSignal::Unknown;
    }
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout {
        use std::io::Read;
        let _ = pipe.read_to_end(&mut stdout);
    }
    let Ok(parsed) = serde_json::from_slice::<ClaudeAuthStatus>(&stdout) else {
        return PlanSignal::Unknown;
    };
    if let Some(name) = parsed
        .subscription_type
        .as_deref()
        .and_then(claude_plan_name)
    {
        return PlanSignal::Named(name);
    }
    if auth_method_is_api(parsed.auth_method.as_deref()) {
        return PlanSignal::Api;
    }
    PlanSignal::Unknown
}

fn auth_method_is_api(method: Option<&str>) -> bool {
    let Some(method) = method else {
        return false;
    };
    let method = method.to_ascii_lowercase();
    method.contains("api") || method == "console"
}

fn claude_plan_name(organization_type: &str) -> Option<String> {
    let kind = organization_type.to_ascii_lowercase();
    let name = if kind.contains("enterprise") {
        "Enterprise"
    } else if kind.contains("team") {
        "Team"
    } else if kind.contains("max") {
        "Max"
    } else if kind.contains("pro") {
        "Pro"
    } else if kind.contains("free") {
        "Free"
    } else {
        return None;
    };
    Some(name.to_string())
}

/// Max is sold as 5x and 20x of Pro; only the rate-limit tier tells them apart.
fn max_tier(account: &ClaudeAccount) -> Option<&'static str> {
    [
        account.organization_rate_limit_tier.as_deref(),
        account.user_rate_limit_tier.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(|tier| {
        let tier = tier.to_ascii_lowercase();
        if tier.contains("max_20x") {
            Some("Max 20x")
        } else if tier.contains("max_5x") {
            Some("Max 5x")
        } else {
            None
        }
    })
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(default, rename = "oauthAccount")]
    oauth_account: Option<ClaudeAccount>,
}

#[derive(Deserialize)]
struct ClaudeAccount {
    #[serde(default, rename = "organizationType")]
    organization_type: Option<String>,
    #[serde(default, rename = "billingType")]
    billing_type: Option<String>,
    #[serde(default, rename = "organizationRateLimitTier")]
    organization_rate_limit_tier: Option<String>,
    #[serde(default, rename = "userRateLimitTier")]
    user_rate_limit_tier: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeAuthStatus {
    #[serde(default, rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(default, rename = "authMethod")]
    auth_method: Option<String>,
}

pub(crate) fn detect_grok() -> PlanSignal {
    let Some(home) = std::env::var_os("HOME") else {
        return PlanSignal::Unknown;
    };
    let home = PathBuf::from(home);
    if let Ok(text) = fs::read_to_string(home.join(".grok/settings_cache.json")) {
        if let Some(name) = grok_tier_from_settings(&text) {
            return PlanSignal::Named(name);
        }
    }
    if grok_auth_is_api_only(&home) {
        return PlanSignal::Api;
    }
    PlanSignal::Unknown
}

fn grok_auth_is_api_only(home: &Path) -> bool {
    let Ok(text) = fs::read_to_string(home.join(".grok/auth.json")) else {
        return false;
    };
    // Only the auth mode is kept. The file's tokens are not deserialized.
    let Ok(entries) =
        serde_json::from_str::<std::collections::BTreeMap<String, GrokAuthEntry>>(&text)
    else {
        return false;
    };
    let mut saw_oidc = false;
    let mut saw_other = false;
    for entry in entries.values() {
        match entry.auth_mode.as_deref() {
            Some("oidc") => saw_oidc = true,
            Some(_) => saw_other = true,
            None => {}
        }
    }
    saw_other && !saw_oidc
}

#[derive(Deserialize)]
struct GrokAuthEntry {
    #[serde(default)]
    auth_mode: Option<String>,
}

fn grok_tier_from_settings(text: &str) -> Option<String> {
    let outer: GrokSettingsFile = serde_json::from_str(text).ok()?;
    let payload = outer.payload?;
    let inner: GrokSettingsInner = serde_json::from_str(&payload).ok()?;
    let name = inner.settings?.subscription_tier_display?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return None;
    }
    Some(name.to_string())
}

#[derive(Deserialize)]
struct GrokSettingsFile {
    #[serde(default)]
    payload: Option<String>,
}

#[derive(Deserialize)]
struct GrokSettingsInner {
    #[serde(default)]
    settings: Option<GrokSettings>,
}

#[derive(Deserialize)]
struct GrokSettings {
    #[serde(default)]
    subscription_tier_display: Option<String>,
}

pub(crate) fn detect_codex() -> PlanSignal {
    let Some(home) = std::env::var_os("HOME") else {
        return PlanSignal::Unknown;
    };
    let path = PathBuf::from(home).join(".codex/auth.json");
    let Ok(text) = fs::read_to_string(path) else {
        return PlanSignal::Unknown;
    };
    codex_plan_from_auth(&text)
}

fn codex_plan_from_auth(text: &str) -> PlanSignal {
    let Ok(auth) = serde_json::from_str::<CodexAuth>(text) else {
        return PlanSignal::Unknown;
    };
    if let Some(mode) = auth.auth_mode.as_deref() {
        if !mode.is_empty() && !mode.eq_ignore_ascii_case("chatgpt") {
            return PlanSignal::Api;
        }
    }
    let from_jwt = auth
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.access_token.as_deref())
        .and_then(jwt_plan_type);
    let from_account = auth
        .account
        .as_ref()
        .and_then(|account| account.plan_type.as_deref())
        .and_then(capitalize_plan);
    match from_jwt.or(from_account) {
        Some(name) => PlanSignal::Named(name),
        None => PlanSignal::Unknown,
    }
}

#[derive(Deserialize)]
struct CodexAuth {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(default)]
    account: Option<CodexAccount>,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct CodexAccount {
    #[serde(default)]
    plan_type: Option<String>,
}

fn jwt_plan_type(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let mut padded = payload.replace('-', "+").replace('_', "/");
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    let bytes = simple_base64_decode(&padded)?;
    let claims: JwtClaims = serde_json::from_slice(&bytes).ok()?;
    let plan = claims.auth?.chatgpt_plan_type?;
    capitalize_plan(&plan)
}

#[derive(Deserialize)]
struct JwtClaims {
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<JwtAuthClaim>,
}

#[derive(Deserialize)]
struct JwtAuthClaim {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

fn capitalize_plan(plan: &str) -> Option<String> {
    let plan = plan.trim();
    if plan.is_empty() || plan.chars().count() > 40 {
        return None;
    }
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().collect::<String>() + chars.as_str())
}

fn simple_base64_decode(input: &str) -> Option<Vec<u8>> {
    const B64_TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buffer = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        let val = B64_TABLE.iter().position(|&x| x == byte)? as u32;
        acc = (acc << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buffer.push((acc >> bits) as u8);
        }
    }
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(provider: &str, name: &str) -> (Option<String>, Option<f64>) {
        let plan = show(provider, PlanSignal::Named(name.to_string()));
        (plan.header, plan.monthly_usd)
    }

    #[test]
    fn plan_shows_detected_name_and_only_a_list_price() {
        assert_eq!(
            shown("claude", "Pro"),
            (Some("Pro $20/mo".into()), Some(20.0))
        );
        assert_eq!(
            shown("claude", "Max 20x"),
            (Some("Max 20x $200/mo".into()), Some(200.0))
        );
        assert_eq!(shown("claude", "Max"), (Some("Max".into()), None));
        assert_eq!(shown("codex", "Pro"), (Some("Pro".into()), None));
        assert_eq!(
            shown("grok", "SuperGrok Heavy"),
            (Some("SuperGrok Heavy".into()), None)
        );
        assert_eq!(show("gemini", PlanSignal::Unknown).header, None);
        assert_eq!(show("codex", PlanSignal::Api).header, None);

        let account: ClaudeAccount = serde_json::from_str(
            r#"{"organizationType":"claude_max","organizationRateLimitTier":"default_claude_max_5x"}"#,
        )
        .expect("account");
        assert_eq!(max_tier(&account), Some("Max 5x"));
        assert_eq!(claude_plan_name("claude_pro").as_deref(), Some("Pro"));
        assert_eq!(claude_plan_name("default_claude_ai"), None);

        let grok = grok_tier_from_settings(
            r#"{"payload":"{\"settings\":{\"subscription_tier_display\":\"SuperGrok Heavy\"}}"}"#,
        );
        assert_eq!(grok.as_deref(), Some("SuperGrok Heavy"));
    }
}
