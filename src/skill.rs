//! The skill embedded in the binary, and the install into one harness folder.
//!
//! A symlink on the way to `SKILL.md` is not written through. The prompt names
//! the resolved file, and the write uses that path.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

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
    let path = confirm_path(&skills.join("crew").join("SKILL.md"));
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
    if !dir.is_dir() {
        return Err(Error::new(
            2,
            format!("{name} has no skills folder at {}", dir.display()),
        ));
    }
    Ok(dir)
}

/// Path shown in the prompt. A symlink in the existing prefix is replaced
/// by its target so the question names the file that would change.
fn confirm_path(path: &Path) -> PathBuf {
    let mut prefix = path.to_path_buf();
    let mut missing = Vec::new();
    while prefix.symlink_metadata().is_err() {
        let Some(parent) = prefix.parent() else {
            return path.to_path_buf();
        };
        if parent.as_os_str().is_empty() || parent == prefix {
            return path.to_path_buf();
        }
        let Some(name) = prefix.file_name() else {
            return path.to_path_buf();
        };
        missing.push(name.to_os_string());
        prefix = parent.to_path_buf();
    }
    if !has_symlink(&prefix) {
        return path.to_path_buf();
    }
    let Ok(mut resolved) = prefix.canonicalize() else {
        return path.to_path_buf();
    };
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    resolved
}

fn has_symlink(path: &Path) -> bool {
    let mut cur = PathBuf::new();
    for component in path.components() {
        cur.push(component);
        if fs::symlink_metadata(&cur).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlinked_destination_is_the_path_we_ask_about() {
        let root = std::env::temp_dir().join(format!("crew-skill-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let real = root.join("real");
        let skills = root.join("skills");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&real, skills.join("crew")).unwrap();
        let asked = confirm_path(&skills.join("crew").join("SKILL.md"));
        let expected = real.canonicalize().unwrap().join("SKILL.md");
        assert_eq!(asked, expected);
        let plain = std::env::current_dir().unwrap().join("not-a-skill.md");
        assert_eq!(confirm_path(&plain), plain);
        fs::remove_dir_all(&root).unwrap();
    }
}
