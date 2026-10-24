use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Lost,
    Stopped,
    Ok,
    Timeout,
    Unclean,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Lost => "lost",
            Self::Stopped => "stopped",
            Self::Ok => "ok",
            Self::Timeout => "timeout",
            Self::Unclean => "unclean",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct JobUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub name: String,
    pub provider: String,
    /// Model flag passed to the provider CLI after tier resolution.
    /// Absent when that CLI is started without one (grok's account default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<JobUsage>,
}

/// Derive dynamic status from record state and supervisor liveness.
pub fn status_of(record: &RunRecord, is_supervisor_alive: bool) -> Status {
    match record.exit_code {
        None => {
            if is_supervisor_alive {
                Status::Running
            } else {
                Status::Lost
            }
        }
        Some(code) => {
            if record.stopped {
                Status::Stopped
            } else if code == 0 {
                Status::Ok
            } else if code == 124 {
                Status::Timeout
            } else if code == 125 {
                Status::Unclean
            } else {
                Status::Failed
            }
        }
    }
}

/// Validate run name against `^[a-z0-9][a-z0-9._-]{0,63}$`.
pub fn validate_run_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '.' && c != '_' && c != '-' {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_name_validation() {
        assert!(validate_run_name("test1"));
        assert!(validate_run_name("a.b-c_d"));
        assert!(validate_run_name("0job"));

        assert!(!validate_run_name(""));
        assert!(!validate_run_name("-bad"));
        assert!(!validate_run_name(".bad"));
        assert!(!validate_run_name("Bad"));
        assert!(!validate_run_name("bad/name"));
        assert!(!validate_run_name(&"a".repeat(65)));
    }

    #[test]
    fn status_derivation() {
        let mut rec = RunRecord {
            name: "test".to_string(),
            provider: "claude".to_string(),
            model: None,
            args: vec![],
            cwd: "/tmp".to_string(),
            note: None,
            parent: None,
            resume_session: None,
            created_at: "2026-10-20T20:00:00Z".to_string(),
            started_at: None,
            finished_at: None,
            supervisor_pid: Some(1234),
            supervisor_start: Some("start".to_string()),
            wrapper_pid: None,
            session_id: None,
            exit_code: None,
            stopped: false,
            usage: None,
        };

        assert_eq!(status_of(&rec, true), Status::Running);
        assert_eq!(status_of(&rec, false), Status::Lost);

        rec.exit_code = Some(0);
        assert_eq!(status_of(&rec, false), Status::Ok);

        rec.exit_code = Some(124);
        assert_eq!(status_of(&rec, false), Status::Timeout);

        rec.exit_code = Some(125);
        assert_eq!(status_of(&rec, false), Status::Unclean);

        rec.exit_code = Some(1);
        assert_eq!(status_of(&rec, false), Status::Failed);

        rec.stopped = true;
        rec.exit_code = Some(143);
        assert_eq!(status_of(&rec, false), Status::Stopped);
    }

    #[test]
    fn serde_rejects_unknown_fields() {
        let json = r#"{
            "name": "test",
            "provider": "claude",
            "args": [],
            "cwd": "/tmp",
            "created_at": "2026-10-20T20:00:00Z",
            "stopped": false,
            "extra_field": 123
        }"#;

        let res = serde_json::from_str::<RunRecord>(json);
        assert!(res.is_err());
    }
}
