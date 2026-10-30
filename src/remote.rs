//! Remote host dispatch over SSH.
//!
//! When invoked as `crew @<host> <args...>`, crew forwards the command
//! to the remote machine defined under `[hosts.<host>]` in config.toml.

use crate::config;
use crate::error::Error;

/// Wrap an argument in POSIX single quotes and escape embedded single quotes.
///
/// Single quotes preserve all characters literally in POSIX shells (spaces,
/// newlines, dollar signs, double quotes) without unwanted expansions.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Assemble the shell command to execute on the remote machine.
///
/// Prepend PATH entries so non-interactive SSH sessions can find crew
/// and harness binaries that are normally set up in the login shell.
pub fn build_remote_command(path: &[String], args: &[String]) -> String {
    let mut cmd = String::new();
    if !path.is_empty() {
        cmd.push_str("PATH=");
        cmd.push_str(&path.join(":"));
        cmd.push_str(":$PATH ");
    }
    cmd.push_str("crew");
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(arg));
    }
    cmd
}

/// Execute the remote command over SSH, forwarding stdio and exit status.
fn run_ssh(ssh_target: &str, remote_cmd: &str, host: &str) -> Result<i32, Error> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.arg(ssh_target);
    cmd.arg(remote_cmd);
    // Direct inheritance allows streaming output, real-time SESSION_ID lines,
    // and interactive stdin without buffering.
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    let status = match cmd.status() {
        Ok(status) => status,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::new(127, "[crew] ssh CLI not found on PATH"));
        }
        Err(err) => {
            return Err(Error::new(127, format!("failed to execute ssh: {err}")));
        }
    };

    let code = status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }
        #[cfg(not(unix))]
        {
            1
        }
    });

    // SSH exits with 255 when the connection or authentication fails.
    if code == 255 {
        eprintln!("[crew] ssh to {host} failed");
    }

    Ok(code)
}

/// Intercept a leading `@host` argument before clap parses local flags.
pub fn maybe_dispatch(args: &[String]) -> Result<Option<i32>, Error> {
    let Some(first) = args.get(1) else {
        return Ok(None);
    };
    if !first.starts_with('@') {
        return Ok(None);
    }
    let host = &first[1..];
    let config = config::load_user()?;
    let hosts = config.as_ref().map(|c| &c.hosts);
    let Some(hosts) = hosts.filter(|h| !h.is_empty()) else {
        return Err(Error::new(
            2,
            format!("unknown host '@{host}': no [hosts] table in config.toml"),
        ));
    };
    let Some(spec) = hosts.get(host) else {
        let names: Vec<&str> = hosts.keys().map(String::as_str).collect();
        return Err(Error::new(
            2,
            format!(
                "unknown host '@{host}': configured hosts are {}",
                names.join(", ")
            ),
        ));
    };

    let remote_cmd = build_remote_command(&spec.path, &args[2..]);
    let code = run_ssh(&spec.ssh, &remote_cmd, host)?;
    Ok(Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_handles_spaces_quotes_newlines_and_dollar() {
        // Spaces
        assert_eq!(shell_quote("hello world"), "'hello world'");
        // Quotes
        assert_eq!(shell_quote("it's a test"), r"'it'\''s a test'");
        assert_eq!(shell_quote("\"quoted\""), "'\"quoted\"'");
        // Newlines
        assert_eq!(shell_quote("line1\nline2"), "'line1\nline2'");
        // Dollar signs
        assert_eq!(shell_quote("$VAR"), "'$VAR'");
        assert_eq!(shell_quote("${HOME}"), "'${HOME}'");
        // Empty string
        assert_eq!(shell_quote(""), "''");

        // Remote command assembly combines PATH prefix and quoted args
        let path = vec!["~/.cargo/bin".to_string(), "~/.local/bin".to_string()];
        let args = vec![
            "job".to_string(),
            "run".to_string(),
            "demo".to_string(),
            "grok".to_string(),
            "-C".to_string(),
            "/data/my path".to_string(),
            "say $HELLO\n'quoted'".to_string(),
        ];
        let remote_cmd = build_remote_command(&path, &args);
        assert_eq!(
            remote_cmd,
            "PATH=~/.cargo/bin:~/.local/bin:$PATH crew 'job' 'run' 'demo' 'grok' '-C' '/data/my path' 'say $HELLO\n'\\''quoted'\\'''"
        );
    }
}
