use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// Which agent binary a command is driving.
///
/// Aliases exist because Vladko asked for the names he already says
/// (`agy`, `gpt`) without a second subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Gemini,
    Grok,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Grok => "grok",
            Self::Codex => "codex",
        }
    }

    /// Names clap accepts, including aliases. Canonical name first.
    pub fn names(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude"],
            Self::Gemini => &["gemini", "antigravity", "agy"],
            Self::Grok => &["grok"],
            Self::Codex => &["codex", "gpt", "chatgpt"],
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "gemini" | "antigravity" | "agy" => Ok(Self::Gemini),
            "grok" => Ok(Self::Grok),
            "codex" | "gpt" | "chatgpt" => Ok(Self::Codex),
            other => Err(Error::new(
                2,
                format!("unknown provider {other} (claude, gemini, grok, codex)"),
            )),
        }
    }
}

/// Effort words every wrapper already accepts. Rewrites happen in the runner.
pub const EFFORTS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

pub fn parse_effort(value: &str) -> Result<(), Error> {
    if EFFORTS.contains(&value) {
        Ok(())
    } else {
        Err(Error::new(
            2,
            format!("effort must be one of {}", EFFORTS.join(", ")),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_map_to_one_provider() {
        assert_eq!("agy".parse::<Provider>().unwrap(), Provider::Gemini);
        assert_eq!("antigravity".parse::<Provider>().unwrap(), Provider::Gemini);
        assert_eq!("gpt".parse::<Provider>().unwrap(), Provider::Codex);
        assert_eq!("chatgpt".parse::<Provider>().unwrap(), Provider::Codex);
        assert!("claude-run".parse::<Provider>().is_err());
    }
}
