use std::io::Read;
use std::process::{Command, Stdio};

const CREW_BIN: &str = env!("CARGO_BIN_EXE_crew");

#[test]
fn broken_pipe_exits_quietly_without_panic() {
    for args in [&["load"][..], &["models"][..], &["skill"][..]] {
        let mut child = Command::new(CREW_BIN)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn crew");
        let mut out = child.stdout.take().unwrap();
        let mut one = [0u8; 1];
        let _ = out.read(&mut one);
        // Close pipe early before the child finishes writing, like `| head -n 1`.
        drop(out);
        let output = child.wait_with_output().expect("wait crew");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "crew {args:?} failed: status={} stderr={stderr}",
            output.status
        );
        assert!(
            !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
            "crew {args:?} panicked on broken pipe: {stderr}"
        );
    }
}
