//! Plan name and monthly price shown by `crew usage`.
//!
//! A name read from the CLI wins. The dollar figure is the public monthly
//! list price only when that exact plan has one; otherwise it comes from
//! ~/.config/crew/plans.toml. An API-key login is not presented as a subscription.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

pub(crate) enum PlanSignal {
    Named(String),
    /// Signed in with an API key. A config row must not invent a subscription.
    Api,
    Unknown,
}

pub(crate) struct ShownPlan {
    pub header: Option<String>,
    pub monthly_usd: Option<f64>,
}

struct PlanConfig {
    name: Option<String>,
    usd_per_month: Option<f64>,
}

pub(crate) struct Plans {
    claude: Option<PlanConfig>,
    codex: Option<PlanConfig>,
    grok: Option<PlanConfig>,
    gemini: Option<PlanConfig>,
}

impl Plans {
    fn get(&self, provider: &str) -> Option<&PlanConfig> {
        match provider {
            "claude" => self.claude.as_ref(),
            "codex" => self.codex.as_ref(),
            "grok" => self.grok.as_ref(),
            "gemini" => self.gemini.as_ref(),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlansFile {
    #[serde(default)]
    claude: Option<PlanRow>,
    #[serde(default)]
    codex: Option<PlanRow>,
    #[serde(default)]
    grok: Option<PlanRow>,
    #[serde(default)]
    gemini: Option<PlanRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRow {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    usd_per_month: Option<f64>,
}

pub(crate) fn load() -> Result<Plans, Error> {
    let Some(path) = plans_path() else {
        return Ok(Plans {
            claude: None,
            codex: None,
            grok: None,
            gemini: None,
        });
    };
    let text = fs::read_to_string(&path).map_err(|err| {
        Error::new(
            2,
            format!("cannot read plans config {}: {err}", path.display()),
        )
    })?;
    parse_plans(&text, &path.display().to_string())
}

fn plans_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CREW_PLANS") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".config/crew/plans.toml");
    path.is_file().then_some(path)
}

fn parse_plans(text: &str, origin: &str) -> Result<Plans, Error> {
    let file: PlansFile = toml::from_str(text)
        .map_err(|err| Error::new(2, format!("plans config {origin} is invalid: {err}")))?;
    Ok(Plans {
        claude: checked_row(file.claude, "claude", origin)?,
        codex: checked_row(file.codex, "codex", origin)?,
        grok: checked_row(file.grok, "grok", origin)?,
        gemini: checked_row(file.gemini, "gemini", origin)?,
    })
}

fn checked_row(
    row: Option<PlanRow>,
    provider: &str,
    origin: &str,
) -> Result<Option<PlanConfig>, Error> {
    let Some(row) = row else {
        return Ok(None);
    };
    let name = match row.name {
        Some(raw) => {
            let name = raw.trim();
            if name.is_empty() {
                return Err(Error::new(
                    2,
                    format!("plans config {origin}: {provider} name is empty"),
                ));
            }
            if name.chars().count() > 80 {
                return Err(Error::new(
                    2,
                    format!("plans config {origin}: {provider} name is too long"),
                ));
            }
            Some(name.to_string())
        }
        None => None,
    };
    if let Some(usd) = row.usd_per_month {
        if !usd.is_finite() || usd < 0.0 || usd >= 100_000.0 {
            return Err(Error::new(
                2,
                format!("plans config {origin}: {provider} usd_per_month is not a monthly price"),
            ));
        }
    }
    Ok(Some(PlanConfig {
        name,
        usd_per_month: row.usd_per_month,
    }))
}

/// Public monthly USD for a plan that has one list price.
///
/// Checked 2026-10-21. Codex "Pro" is several prices, Claude Max is 5x or 20x,
/// and SuperGrok / Google AI tiers are not a single number, so they are absent.
fn list_price(provider: &str, name: &str) -> Option<f64> {
    match (provider, name.to_ascii_lowercase().as_str()) {
        ("claude", "pro") => Some(20.0),
        ("claude", "free") => Some(0.0),
        ("codex", "plus") => Some(20.0),
        ("codex", "go") => Some(8.0),
        ("codex", "free") => Some(0.0),
        _ => None,
    }
}

pub(crate) fn show(provider: &str, signal: PlanSignal, plans: &Plans) -> ShownPlan {
    if matches!(signal, PlanSignal::Api) {
        return ShownPlan {
            header: None,
            monthly_usd: None,
        };
    }
    let config = plans.get(provider);
    let (name, name_from_config) = match signal {
        PlanSignal::Named(raw) if !raw.trim().is_empty() => (Some(raw.trim().to_string()), false),
        _ => match config.and_then(|row| row.name.clone()) {
            Some(name) => (Some(name), true),
            None => (None, false),
        },
    };
    let list = name.as_deref().and_then(|n| list_price(provider, n));
    let (price, price_from_config) = if let Some(usd) = list {
        (Some(usd), false)
    } else if let Some(usd) = config.and_then(|row| row.usd_per_month) {
        (Some(usd), true)
    } else {
        (None, false)
    };
    if name.is_none() && price.is_none() {
        return ShownPlan {
            header: None,
            monthly_usd: None,
        };
    }
    let mut header = String::new();
    if let Some(ref n) = name {
        header.push_str(n);
    }
    if let Some(usd) = price {
        if !header.is_empty() {
            header.push(' ');
        }
        header.push_str(&format_usd(usd));
        header.push_str("/mo");
    }
    if name_from_config || (name.is_none() && price_from_config) {
        header.push_str(" (from config)");
    } else if price_from_config {
        header.push_str(" (price from config)");
    }
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
                        return PlanSignal::Named(name);
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

    #[test]
    fn plan_resolution_prefers_detection_and_list_price() {
        let plans = parse_plans(
            r#"
            [claude]
            name = "Free"

            [codex]
            name = "Plus"
            usd_per_month = 200

            [gemini]
            name = "Google AI Pro"
            usd_per_month = 20
            "#,
            "test",
        )
        .expect("plans");

        let claude = show("claude", PlanSignal::Named("Pro".to_string()), &plans);
        assert_eq!(claude.header.as_deref(), Some("Pro $20/mo"));
        assert_eq!(claude.monthly_usd, Some(20.0));

        let priced = parse_plans("[claude]\nusd_per_month = 999\n", "test").expect("priced");
        let kept = show("claude", PlanSignal::Named("Pro".to_string()), &priced);
        assert_eq!(kept.header.as_deref(), Some("Pro $20/mo"));
        assert_eq!(kept.monthly_usd, Some(20.0));

        let max = show("claude", PlanSignal::Named("Max".to_string()), &plans);
        assert_eq!(max.header.as_deref(), Some("Max"));
        assert_eq!(max.monthly_usd, None);

        let codex = show("codex", PlanSignal::Named("Pro".to_string()), &plans);
        assert_eq!(
            codex.header.as_deref(),
            Some("Pro $200/mo (price from config)")
        );
        assert_eq!(codex.monthly_usd, Some(200.0));

        let gemini = show("gemini", PlanSignal::Unknown, &plans);
        assert_eq!(
            gemini.header.as_deref(),
            Some("Google AI Pro $20/mo (from config)")
        );

        let api = show("codex", PlanSignal::Api, &plans);
        assert_eq!(api.header, None);
        assert_eq!(api.monthly_usd, None);

        assert_eq!(claude_plan_name("claude_pro").as_deref(), Some("Pro"));
        assert_eq!(claude_plan_name("claude_max").as_deref(), Some("Max"));
        assert_eq!(claude_plan_name("default_claude_ai"), None);

        let grok = grok_tier_from_settings(
            r#"{"payload":"{\"settings\":{\"subscription_tier_display\":\"SuperGrok Heavy\"}}"}"#,
        );
        assert_eq!(grok.as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(
            grok_tier_from_settings(
                r#"{"payload":"{\"settings\":{\"subscription_tier_display\":\"\"}}"}"#
            ),
            None
        );

        assert!(parse_plans("[codex]\nusd_per_month = -1\n", "test").is_err());
        assert!(parse_plans("[nope]\nname = \"x\"\n", "test").is_err());
    }
}
