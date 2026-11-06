//! The skill embedded in the binary, and the install into one harness folder.
//!
//! The prompt and the write use the harness default path. A missing skills
//! folder is created with the `crew` directory.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use crate::error::Error;

/// Text of `skill/SKILL.md`, frontmatter included.
pub const TEXT: &str = include_str!("../skill/SKILL.md");

const HARNESSES: &[&str] = &["claude", "codex", "grok", "gemini", "pi"];

pub fn run(harness: Option<&str>) -> Result<i32, Error> {
    match harness {
        None => {
            print!("{TEXT}");
            Ok(0)
        }
        Some(name) => install(name),
    }
}

fn install(name: &str) -> Result<i32, Error> {
    // A pipe must not answer the prompt. The skill text is `crew skill` with no harness.
    if !io::stdin().is_terminal() {
        return Err(Error::new(
            2,
            "run it in a terminal, or use `crew skill > <path>`",
        ));
    }
    let skills = skills_dir(name)?;
    let path = skills.join("crew").join("SKILL.md");
    eprint!("Write {}? [Y/n] ", path.display());
    io::stderr().flush().ok();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 || !accepted(&line) {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| Error::new(2, format!("cannot create {}: {err}", parent.display())))?;
    }
    fs::write(&path, TEXT)
        .map_err(|err| Error::new(2, format!("cannot write {}: {err}", path.display())))?;
    println!("{}", path.display());
    Ok(0)
}

fn accepted(line: &str) -> bool {
    let answer = line.trim();
    answer.is_empty() || answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

fn skills_dir(name: &str) -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::new(2, "HOME is not set"))?;
    let dir = match name {
        "claude" => home.join(".claude/skills"),
        "codex" => match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("skills"),
            None => home.join(".codex/skills"),
        },
        "grok" => home.join(".grok/skills"),
        "gemini" => home.join(".gemini/config/skills"),
        "pi" => home.join(".pi/agent/skills"),
        _ => {
            return Err(Error::new(
                2,
                format!("unknown harness {name} ({})", HARNESSES.join(", ")),
            ));
        }
    };
    Ok(dir)
}
