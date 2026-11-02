use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::config::{self, AgentSpec, UserTier};
use crate::error::Error;
use crate::id::{EFFORTS, Provider};

const BUILTIN: &str = include_str!("../tiers.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Fast,
    Balanced,
    Smart,
    Max,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Smart => "smart",
            Self::Max => "max",
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Fast, Self::Balanced, Self::Smart, Self::Max]
    }
}

pub fn parse_name(name: &str) -> Option<Tier> {
    match name {
        "fast" => Some(Tier::Fast),
        "balanced" => Some(Tier::Balanced),
        "smart" => Some(Tier::Smart),
        "max" => Some(Tier::Max),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// `None` means the runner omits the model flag. Grok does that when no
    /// tier was given. A pi agent does it when no model and no balanced row.
    pub model: Option<String>,
    pub effort: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderRows {
    fast: Row,
    balanced: Row,
    smart: Row,
    max: Row,
}

impl ProviderRows {
    fn get(&self, tier: Tier) -> &Row {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Balanced => &self.balanced,
            Tier::Smart => &self.smart,
            Tier::Max => &self.max,
        }
    }

    fn get_mut(&mut self, tier: Tier) -> &mut Row {
        match tier {
            Tier::Fast => &mut self.fast,
            Tier::Balanced => &mut self.balanced,
            Tier::Smart => &mut self.smart,
            Tier::Max => &mut self.max,
        }
    }
}

/// A full table. A partial user file is an error so a typo cannot silently
/// fall back to the built-in row for the tier it forgot.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    claude: ProviderRows,
    codex: ProviderRows,
    gemini: ProviderRows,
    grok: ProviderRows,
}

impl Table {
    fn provider(&self, provider: Provider) -> &ProviderRows {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
            Provider::Gemini => &self.gemini,
            Provider::Grok => &self.grok,
            // pi rows are not in the built-in table. Callers use `pi_settings`.
            Provider::Pi => panic!("pi has no built-in tier row"),
        }
    }

    fn provider_mut(&mut self, provider: Provider) -> &mut ProviderRows {
        match provider {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
            Provider::Gemini => &mut self.gemini,
            Provider::Grok => &mut self.grok,
            Provider::Pi => panic!("pi has no built-in tier row"),
        }
    }
}

struct Loaded {
    table: Table,
    /// Built-in rows the user file replaced. The rest stay `builtin`.
    overridden: BTreeSet<(String, String)>,
    /// pi and custom agents. They have no built-in rows.
    extras: BTreeMap<String, BTreeMap<Tier, UserTier>>,
    agents: BTreeMap<String, AgentSpec>,
    effort: BTreeMap<String, BTreeMap<String, String>>,
}

fn parse_table(text: &str, origin: &str) -> Result<Table, Error> {
    let table: Table = toml::from_str(text)
        .map_err(|err| Error::new(2, format!("tier config {origin} is invalid: {err}")))?;
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Gemini,
        Provider::Grok,
    ] {
        for tier in Tier::all() {
            let row = table.provider(provider).get(tier);
            if row.model.is_empty() {
                return Err(Error::new(
                    2,
                    format!(
                        "tier config {origin}: {provider} {} has an empty model",
                        tier.as_str()
                    ),
                ));
            }
        }
    }
    Ok(table)
}

fn load() -> Result<Loaded, Error> {
    let mut loaded = Loaded {
        table: parse_table(BUILTIN, "builtin")?,
        overridden: BTreeSet::new(),
        extras: BTreeMap::new(),
        agents: BTreeMap::new(),
        effort: BTreeMap::new(),
    };
    let Some(user) = config::load_user()? else {
        return Ok(loaded);
    };
    loaded.agents = user.agents;
    loaded.effort = user.effort;
    for (owner, rows) in user.tiers {
        for (tier_name, row) in rows {
            let Some(tier) = parse_name(&tier_name) else {
                continue;
            };
            // pi is a provider name, but its rows are not in the built-in table.
            if owner != "pi"
                && let Ok(provider) = owner.parse::<Provider>()
            {
                let slot = loaded.table.provider_mut(provider).get_mut(tier);
                slot.model = row.model;
                loaded.overridden.insert((owner.clone(), tier_name));
            } else {
                loaded
                    .extras
                    .entry(owner.clone())
                    .or_default()
                    .insert(tier, row);
            }
        }
    }
    Ok(loaded)
}

/// Config wire word for this owner and user word, if the file remaps it.
///
/// The owner is the canonical provider or the custom agent name. An alias
/// such as `agy` is not a key, and a custom agent does not inherit `[effort.pi]`.
pub fn effort_override(owner: &str, effort: &str) -> Result<Option<String>, Error> {
    let loaded = load()?;
    Ok(loaded
        .effort
        .get(owner)
        .and_then(|words| words.get(effort))
        .cloned())
}

/// What the runner should put on the CLI.
///
/// No tier and no `-m` uses the balanced row, so a config override of that
/// row is also the model. Effort is the word the user typed, or `medium`.
/// A tier never changes it. Grok still omits the model flag when both are
/// absent, so the account default stands.
pub fn resolve(
    provider: Provider,
    tier: Option<Tier>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Resolved, Error> {
    if provider == Provider::Pi {
        return Err(Error::new(
            2,
            "pi has no built-in model default. Pass -m, or add a tier row to config.toml",
        ));
    }
    let loaded = load()?;
    resolve_with(&loaded.table, provider, tier, model, effort)
}

/// Model and thinking for `pi` or a pi-based agent.
///
/// `thinking` is always set. With no tier and no `-m`, the balanced row is
/// the model when the file has one. Otherwise no model is sent.
pub struct PiSettings {
    pub model: Option<String>,
    pub thinking: String,
}

pub fn agent(name: &str) -> Result<Option<AgentSpec>, Error> {
    Ok(load()?.agents.get(name).cloned())
}

pub fn agent_names() -> Result<Vec<String>, Error> {
    Ok(load()?.agents.keys().cloned().collect())
}

pub fn pi_settings(
    name: &str,
    tier: Option<Tier>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<PiSettings, Error> {
    if let Some(effort) = effort
        && !EFFORTS.contains(&effort)
    {
        return Err(Error::new(
            2,
            format!("effort must be one of {}", EFFORTS.join(", ")),
        ));
    }
    let loaded = load()?;
    let row = match tier {
        Some(tier) => Some(
            loaded
                .extras
                .get(name)
                .and_then(|rows| rows.get(&tier))
                .ok_or_else(|| {
                    Error::new(
                        2,
                        format!(
                            "no [tiers.{name}.{}] in {}. Add [tiers.{name}.{}] to config.toml",
                            tier.as_str(),
                            config::config_path().display(),
                            tier.as_str()
                        ),
                    )
                })?,
        ),
        None => loaded
            .extras
            .get(name)
            .and_then(|rows| rows.get(&Tier::Balanced)),
    };
    let model = match model {
        Some(model) => Some(model.to_string()),
        None => row.map(|row| row.model.clone()),
    };
    Ok(PiSettings {
        model,
        thinking: effort.unwrap_or(DEFAULT_EFFORT).to_string(),
    })
}

fn resolve_with(
    table: &Table,
    provider: Provider,
    tier: Option<Tier>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Resolved, Error> {
    if let Some(effort) = effort
        && !EFFORTS.contains(&effort)
    {
        return Err(Error::new(
            2,
            format!("effort must be one of {}", EFFORTS.join(", ")),
        ));
    }
    // No tier uses balanced, except grok, which sends no model at all.
    let model = if let Some(model) = model {
        Some(model.to_string())
    } else if provider == Provider::Grok && tier.is_none() {
        None
    } else {
        let tier = tier.unwrap_or(Tier::Balanced);
        Some(table.provider(provider).get(tier).model.clone())
    };
    Ok(Resolved {
        model,
        effort: effort.unwrap_or(DEFAULT_EFFORT).to_string(),
    })
}

/// Effort when the command names none. Every provider sends this.
pub const DEFAULT_EFFORT: &str = "medium";

pub fn print() -> Result<i32, Error> {
    let loaded = load()?;
    println!("provider    tier      model                      source");
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Gemini,
        Provider::Grok,
    ] {
        for tier in Tier::all() {
            let row = loaded.table.provider(provider).get(tier);
            let source = if loaded
                .overridden
                .contains(&(provider.as_str().to_string(), tier.as_str().to_string()))
            {
                "config"
            } else {
                "builtin"
            };
            print_row(provider.as_str(), tier, &row.model, source);
        }
    }
    for (owner, rows) in &loaded.extras {
        for tier in Tier::all() {
            let Some(row) = rows.get(&tier) else {
                continue;
            };
            print_row(owner, tier, &row.model, "config");
        }
    }
    let mut any_effort = false;
    for (owner, words) in &loaded.effort {
        let mut parts = Vec::new();
        for word in EFFORTS {
            if let Some(wire) = words.get(word) {
                parts.push(format!("{word} -> {wire}"));
            }
        }
        if parts.is_empty() {
            continue;
        }
        if !any_effort {
            println!("\neffort");
            any_effort = true;
        }
        println!("{owner}  {}", parts.join("  "));
    }
    Ok(0)
}

fn print_row(owner: &str, tier: Tier, model: &str, source: &str) {
    println!(
        "{:<10}  {:<8}  {:<25}  {source}",
        owner,
        tier.as_str(),
        model,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_follows_the_builtin_row() {
        let table = parse_table(BUILTIN, "builtin").unwrap();
        for provider in [
            Provider::Claude,
            Provider::Codex,
            Provider::Gemini,
            Provider::Grok,
        ] {
            for tier in Tier::all() {
                let row = table.provider(provider).get(tier);
                let resolved = resolve_with(&table, provider, Some(tier), None, None).unwrap();
                assert_eq!(resolved.model.as_deref(), Some(row.model.as_str()));
                // A tier used to carry its own effort. It must not anymore.
                assert_eq!(resolved.effort, DEFAULT_EFFORT);
            }
        }
    }

    #[test]
    fn unknown_tier_key_is_rejected() {
        let text = r#"
            [claude.fast]
            model = "haiku"
            extra = true
        "#;
        let err = parse_table(text, "test").unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn omitted_tier_uses_the_balanced_row() {
        let table = parse_table(BUILTIN, "builtin").unwrap();
        let grok = resolve_with(&table, Provider::Grok, None, None, None).unwrap();
        assert_eq!(grok.model, None);
        assert_eq!(grok.effort, DEFAULT_EFFORT);
        for provider in [Provider::Claude, Provider::Codex, Provider::Gemini] {
            let resolved = resolve_with(&table, provider, None, None, None).unwrap();
            let balanced = table.provider(provider).get(Tier::Balanced);
            assert_eq!(resolved.model.as_deref(), Some(balanced.model.as_str()));
            // No tier and no effort word is one effort for every provider,
            // including codex, whose own CLI would have used low.
            assert_eq!(resolved.effort, DEFAULT_EFFORT);
        }

        let balanced = table.provider(Provider::Codex).get(Tier::Balanced);
        let fast = table.provider(Provider::Codex).get(Tier::Fast);
        let overridden = BUILTIN.replace(
            &format!("[codex.balanced]\nmodel = \"{}\"", balanced.model),
            "[codex.balanced]\nmodel = \"sentinel-model\"",
        );
        let table = parse_table(&overridden, "test").unwrap();
        let resolved = resolve_with(&table, Provider::Codex, None, None, None).unwrap();
        assert_eq!(resolved.model.as_deref(), Some("sentinel-model"));
        let still_fast =
            resolve_with(&table, Provider::Codex, Some(Tier::Fast), None, None).unwrap();
        assert_eq!(still_fast.model.as_deref(), Some(fast.model.as_str()));
    }

    #[test]
    fn model_flag_overrides_the_tier_model_only() {
        let table = parse_table(BUILTIN, "builtin").unwrap();
        let resolved = resolve_with(
            &table,
            Provider::Claude,
            Some(Tier::Fast),
            Some("opus"),
            None,
        )
        .unwrap();
        let row = table.provider(Provider::Claude).get(Tier::Fast);
        assert_eq!(resolved.model.as_deref(), Some("opus"));
        assert_ne!(resolved.model.as_deref(), Some(row.model.as_str()));
        assert_eq!(resolved.effort, DEFAULT_EFFORT);
    }
}
