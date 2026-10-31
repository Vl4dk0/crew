//! User config at `$XDG_CONFIG_HOME/crew/config.toml`.
//!
//! The built-in tier table stays in the repo. This file overrides single
//! tier rows and defines pi-based agents that should not live in the binary.
//! A missing file is the built-in table alone. A bad file names itself and
//! the key, and the process exits 2.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;
use crate::id::EFFORTS;

/// Subcommands and provider aliases an agent must not take.
const RESERVED: &[&str] = &[
    "claude",
    "gemini",
    "antigravity",
    "agy",
    "grok",
    "codex",
    "gpt",
    "chatgpt",
    "pi",
    "job",
    "load",
    "usage",
    "update",
    "models",
    "help",
];

const PROVIDER_KEYS: &[&str] = &["claude", "codex", "gemini", "grok", "pi"];

const TIER_KEYS: &[&str] = &["fast", "balanced", "smart", "max"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTier {
    pub model: String,
}

/// A pi-based command. `command[0]` is the binary, the rest are fixed args.
/// `~` is expanded when the process is started, not when the file is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    pub ssh: String,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfig {
    pub tiers: BTreeMap<String, BTreeMap<String, UserTier>>,
    pub agents: BTreeMap<String, AgentSpec>,
    /// Owner (canonical provider or custom agent) to user-word to wire word.
    /// The wire word is sent as written. It is not passed through the built-in map.
    pub effort: BTreeMap<String, BTreeMap<String, String>>,
    pub hosts: BTreeMap<String, HostSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    tiers: BTreeMap<String, BTreeMap<String, UserRow>>,
    #[serde(default)]
    agents: BTreeMap<String, AgentToml>,
    #[serde(default)]
    effort: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    hosts: BTreeMap<String, HostToml>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserRow {
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentToml {
    base: String,
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostToml {
    ssh: String,
    #[serde(default)]
    path: Vec<String>,
}

/// `$XDG_CONFIG_HOME/crew/config.toml`, or `~/.config/crew/config.toml`.
/// macOS uses the same path as Linux.
pub fn config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("crew/config.toml");
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".config/crew/config.toml"),
        _ => PathBuf::from(".config/crew/config.toml"),
    }
}

/// `None` when the file is not there. A present file is parsed in full.
pub fn load_user() -> Result<Option<UserConfig>, Error> {
    let path = config_path();
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|err| Error::new(2, format!("config {}: cannot read: {err}", path.display())))?;
    parse_user(&text, &path.display().to_string()).map(Some)
}

pub fn parse_user(text: &str, origin: &str) -> Result<UserConfig, Error> {
    let file: File =
        toml::from_str(text).map_err(|err| Error::new(2, format!("config {origin}: {err}")))?;
    let mut agents = BTreeMap::new();
    for (name, spec) in file.agents {
        let key = format!("agents.{name}");
        if RESERVED.contains(&name.as_str()) {
            return Err(Error::new(
                2,
                format!(
                    "config {origin}: {key}: name collides with a built-in command or provider"
                ),
            ));
        }
        if !valid_agent_name(&name) {
            return Err(Error::new(
                2,
                format!("config {origin}: {key}: name must be a single command word"),
            ));
        }
        if spec.base != "pi" {
            return Err(Error::new(
                2,
                format!("config {origin}: {key}.base: only \"pi\" is supported"),
            ));
        }
        if spec.command.is_empty() || spec.command.iter().any(|part| part.is_empty()) {
            return Err(Error::new(
                2,
                format!("config {origin}: {key}.command: needs the binary and any fixed arguments"),
            ));
        }
        agents.insert(
            name,
            AgentSpec {
                command: spec.command,
            },
        );
    }
    let mut tiers = BTreeMap::new();
    for (owner, rows) in file.tiers {
        let known = PROVIDER_KEYS.contains(&owner.as_str()) || agents.contains_key(&owner);
        if !known {
            return Err(Error::new(
                2,
                format!("config {origin}: tiers.{owner}: unknown provider or agent"),
            ));
        }
        let mut parsed = BTreeMap::new();
        for (tier_name, row) in rows {
            let key = format!("tiers.{owner}.{tier_name}");
            if !TIER_KEYS.contains(&tier_name.as_str()) {
                return Err(Error::new(
                    2,
                    format!(
                        "config {origin}: {key}: tier must be {}",
                        TIER_KEYS.join(", ")
                    ),
                ));
            }
            if row.model.is_empty() {
                return Err(Error::new(
                    2,
                    format!("config {origin}: {key}.model: model is empty"),
                ));
            }
            parsed.insert(tier_name, UserTier { model: row.model });
        }
        tiers.insert(owner, parsed);
    }
    let mut effort = BTreeMap::new();
    for (owner, words) in file.effort {
        let known = PROVIDER_KEYS.contains(&owner.as_str()) || agents.contains_key(&owner);
        if !known {
            return Err(Error::new(
                2,
                format!("config {origin}: effort.{owner}: unknown provider or agent"),
            ));
        }
        let mut parsed = BTreeMap::new();
        for (word, wire) in words {
            let key = format!("effort.{owner}.{word}");
            if !EFFORTS.contains(&word.as_str()) {
                return Err(Error::new(
                    2,
                    format!(
                        "config {origin}: {key}: effort must be {}",
                        EFFORTS.join(", ")
                    ),
                ));
            }
            if wire.is_empty() {
                return Err(Error::new(
                    2,
                    format!("config {origin}: {key}: wire word is empty"),
                ));
            }
            parsed.insert(word, wire);
        }
        effort.insert(owner, parsed);
    }
    let mut hosts = BTreeMap::new();
    for (name, spec) in file.hosts {
        let key = format!("hosts.{name}");
        if !valid_host_name(&name) {
            return Err(Error::new(
                2,
                format!("config {origin}: {key}: name must be a single word"),
            ));
        }
        if spec.ssh.trim().is_empty() {
            return Err(Error::new(
                2,
                format!("config {origin}: {key}.ssh: cannot be empty"),
            ));
        }
        hosts.insert(
            name,
            HostSpec {
                ssh: spec.ssh,
                path: spec.path,
            },
        );
    }
    Ok(UserConfig {
        tiers,
        agents,
        effort,
        hosts,
    })
}

fn valid_host_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_file_overrides_one_row_and_rejects_a_bad_key() {
        let text = r#"
            [tiers.claude.fast]
            model = "model-a"

            [tiers.pi.smart]
            model = "model-b"

            [agents.piprover]
            base = "pi"
            command = ["~/bin/pilocal", "--prover"]

            [tiers.piprover.fast]
            model = "model-c"

            [effort.claude]
            max = "high"

            [effort.piprover]
            low = "high"

            [hosts.home]
            ssh = "home"
            path = ["~/.cargo/bin", "~/.local/bin", "~/.grok/bin"]
        "#;
        let parsed = parse_user(text, "fixture").unwrap();
        // Expectations come from the fixture text, so a fixture edit cannot
        // leave a stale literal behind.
        let raw: File = toml::from_str(text).unwrap();
        assert_eq!(parsed.tiers.len(), raw.tiers.len());
        for (owner, rows) in &raw.tiers {
            let got = &parsed.tiers[owner];
            assert_eq!(got.len(), rows.len());
            for (tier, row) in rows {
                assert_eq!(got[tier].model, row.model);
            }
        }
        assert_eq!(parsed.agents.len(), raw.agents.len());
        for (name, spec) in &raw.agents {
            assert_eq!(parsed.agents[name].command, spec.command);
        }
        assert_eq!(parsed.effort.len(), raw.effort.len());
        for (owner, words) in &raw.effort {
            assert_eq!(&parsed.effort[owner], words);
        }
        assert_eq!(parsed.hosts.len(), raw.hosts.len());
        for (name, spec) in &raw.hosts {
            assert_eq!(parsed.hosts[name].ssh, spec.ssh);
            assert_eq!(parsed.hosts[name].path, spec.path);
        }

        let extra = "[tiers.claude.fast]\nmodel = \"model-a\"\nextra = true\n";
        let err = parse_user(extra, "fixture").unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("fixture"), "{}", err.message);
        assert!(err.message.contains("extra"), "{}", err.message);

        let bad_host_key = "[hosts.home]\nssh = \"home\"\nunknown = true\n";
        let err = parse_user(bad_host_key, "fixture").unwrap_err();
        assert!(err.message.contains("unknown"), "{}", err.message);

        let empty_ssh = "[hosts.home]\nssh = \"\"\n";
        let err = parse_user(empty_ssh, "fixture").unwrap_err();
        assert!(err.message.contains("hosts.home.ssh"), "{}", err.message);

        let collision = "[agents.job]\nbase = \"pi\"\ncommand = [\"pi\"]\n";
        let err = parse_user(collision, "fixture").unwrap_err();
        assert!(err.message.contains("agents.job"), "{}", err.message);

        let base = "[agents.mine]\nbase = \"codex\"\ncommand = [\"pi\"]\n";
        let err = parse_user(base, "fixture").unwrap_err();
        assert!(err.message.contains("agents.mine.base"), "{}", err.message);

        let unknown = "[tiers.nope.fast]\nmodel = \"model-a\"\n";
        let err = parse_user(unknown, "fixture").unwrap_err();
        assert!(err.message.contains("tiers.nope"), "{}", err.message);

        let alias = "[effort.agy]\nmax = \"high\"\n";
        let err = parse_user(alias, "fixture").unwrap_err();
        assert!(err.message.contains("effort.agy"), "{}", err.message);

        let bad_word = "[effort.claude]\nultra = \"high\"\n";
        let err = parse_user(bad_word, "fixture").unwrap_err();
        assert!(
            err.message.contains("effort.claude.ultra"),
            "{}",
            err.message
        );
    }
}
