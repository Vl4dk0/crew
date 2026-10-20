use std::process::Command;

/// Check whether the process with `pid` is currently alive and has the expected `lstart`.
pub fn is_process_alive(pid: u32, expected_lstart: Option<&str>) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let actual = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if actual.is_empty() {
                return false;
            }
            if let Some(exp) = expected_lstart {
                actual == exp
            } else {
                true
            }
        }
        _ => false,
    }
}

/// Retrieve the process start time (`lstart`) for a given PID.
pub fn get_process_lstart(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;

    if output.status.success() {
        let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !val.is_empty() {
            return Some(val);
        }
    }
    None
}
