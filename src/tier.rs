use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;
use crate::id::{Provider, parse_effort};

const BUILTIN: &str = include_str!("../tiers.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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

    fn all() -> [Self; 4] {
        [Self::Fast, Self::Balanced, Self::Smart, Self::Max]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// `None` means the runner omits `-m`. Only the grok wrapper default does that.
    pub model: Option<String>,
    pub effort: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Builtin,
    User,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    model: String,
    /// Absent for grok. A grok tier does not select effort. `-e` does.
    #[serde(default)]
    effort: Option<String>,
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
        }
    }
}

struct Loaded {
    table: Table,
    source: Source,
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
            match (provider, row.effort.as_deref()) {
                (Provider::Grok, Some(_)) => {
                    return Err(Error::new(
                        2,
                        format!(
                            "tier config {origin}: grok {} must not set effort. Pass -e.",
                            tier.as_str()
                        ),
                    ));
                }
                (Provider::Grok, None) => {}
                (_, None) => {
                    return Err(Error::new(
                        2,
                        format!(
                            "tier config {origin}: {provider} {} is missing effort",
                            tier.as_str()
                        ),
                    ));
                }
                (_, Some(effort)) => parse_effort(effort).map_err(|err| {
                    Error::new(
                        2,
                        format!(
                            "tier config {origin}: {provider} {} effort: {}",
                            tier.as_str(),
                            err.message
                        ),
                    )
                })?,
            }
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

fn user_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CREW_TIERS") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".config/crew/tiers.toml");
    path.is_file().then_some(path)
}

fn load() -> Result<Loaded, Error> {
    let Some(path) = user_path() else {
        return Ok(Loaded {
            table: parse_table(BUILTIN, "builtin")?,
            source: Source::Builtin,
        });
    };
    let text = fs::read_to_string(&path).map_err(|err| {
        Error::new(
            2,
            format!("cannot read tier config {}: {err}", path.display()),
        )
    })?;
    Ok(Loaded {
        table: parse_table(&text, &path.display().to_string())?,
        source: Source::User,
    })
}

/// What the runner should put on the CLI.
///
/// No tier and no `-m` keeps the wrapper's model default and uses effort
/// `medium`. It does not apply `balanced`, because grok's wrapper omits `-m`.
pub fn resolve(
    provider: Provider,
    tier: Option<Tier>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Resolved, Error> {
    let loaded = load()?;
    resolve_with(&loaded.table, provider, tier, model, effort)
}

fn resolve_with(
    table: &Table,
    provider: Provider,
    tier: Option<Tier>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Resolved, Error> {
    if let Some(effort) = effort {
        parse_effort(effort)?;
    }
    if tier.is_none() && model.is_none() {
        return Ok(wrapper_default(provider, effort));
    }
    let (row_model, row_effort) = if let Some(tier) = tier {
        let row = table.provider(provider).get(tier);
        let row_effort = if provider == Provider::Grok {
            DEFAULT_EFFORT.to_string()
        } else {
            row.effort.clone().ok_or_else(|| {
                Error::new(2, format!("{provider} {} is missing effort", tier.as_str()))
            })?
        };
        (row.model.clone(), row_effort)
    } else {
        let default = wrapper_default(provider, None);
        (default.model.unwrap_or_default(), default.effort)
    };
    Ok(Resolved {
        model: Some(model.unwrap_or(&row_model).to_string()),
        effort: effort.unwrap_or(&row_effort).to_string(),
    })
}

/// Model a wrapper gets when the user passed no tier and no `-m`.
/// Grok has none: the wrapper omits `-m` and the account default stands.
const CLAUDE_WRAPPER_MODEL: &str = "sonnet";
const CODEX_WRAPPER_MODEL: &str = "gpt-6-luna";
const GEMINI_WRAPPER_MODEL: &str = "gemini-3.8-flash-medium";

/// Effort when the user did not pass `-e`. A grok tier does not carry one.
const DEFAULT_EFFORT: &str = "medium";

fn wrapper_default(provider: Provider, effort: Option<&str>) -> Resolved {
    let model = match provider {
        Provider::Claude => Some(CLAUDE_WRAPPER_MODEL.to_string()),
        Provider::Codex => Some(CODEX_WRAPPER_MODEL.to_string()),
        Provider::Gemini => Some(GEMINI_WRAPPER_MODEL.to_string()),
        Provider::Grok => None,
    };
    Resolved {
        model,
        effort: effort.unwrap_or(DEFAULT_EFFORT).to_string(),
    }
}

pub fn print() -> Result<i32, Error> {
    let loaded = load()?;
    let source = match loaded.source {
        Source::Builtin => "builtin",
        Source::User => "user",
    };
    println!("provider  tier      model                      effort  source");
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Gemini,
        Provider::Grok,
    ] {
        for tier in Tier::all() {
            let row = loaded.table.provider(provider).get(tier);
            let effort = row.effort.as_deref().unwrap_or("-");
            println!(
                "{:<8}  {:<8}  {:<25}  {:<6}  {source}",
                provider.as_str(),
                tier.as_str(),
                row.model,
                effort,
            );
        }
    }
    Ok(0)
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
                if provider == Provider::Grok {
                    assert!(row.effort.is_none());
                    assert_eq!(resolved.effort, DEFAULT_EFFORT);
                } else {
                    assert_eq!(Some(resolved.effort.as_str()), row.effort.as_deref());
                }
            }
        }
    }

    #[test]
    fn unknown_tier_key_is_rejected() {
        let text = r#"
            [claude.fast]
            model = "haiku"
            effort = "medium"
            extra = true
        "#;
        let err = parse_table(text, "test").unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn omitted_tier_keeps_wrapper_defaults() {
        let table = parse_table(BUILTIN, "builtin").unwrap();
        let grok = resolve_with(&table, Provider::Grok, None, None, None).unwrap();
        assert_eq!(grok.model, None);
        assert_eq!(grok.effort, DEFAULT_EFFORT);
        let codex = resolve_with(&table, Provider::Codex, None, None, None).unwrap();
        assert_eq!(codex.model.as_deref(), Some(CODEX_WRAPPER_MODEL));
        // No tier and no -e is one effort for every provider, including
        // codex, whose own wrapper defaults to low. Vladko asked for that.
        assert_eq!(codex.effort, DEFAULT_EFFORT);
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
        assert_eq!(Some(resolved.effort.as_str()), row.effort.as_deref());
    }
}
