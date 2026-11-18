use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const CREW_BIN: &str = env!("CARGO_BIN_EXE_crew");
static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

struct TestEnv {
    root: PathBuf,
    crew_home: PathBuf,
    bin_dir: PathBuf,
}

impl TestEnv {
    fn new(name: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("crew-it-{name}-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crew_home = root.join("home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&crew_home).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        Self {
            root,
            crew_home,
            bin_dir,
        }
    }

    fn write_script(&self, name: &str, body: &str) {
        let path = self.bin_dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }

    fn path_env(&self) -> String {
        let orig = std::env::var("PATH").unwrap_or_default();
        format!("{}:{orig}", self.bin_dir.display())
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(CREW_BIN);
        cmd.args(args)
            .env("CREW_HOME", &self.crew_home)
            .env("PATH", self.path_env());
        cmd
    }

    fn run_with_stdin(&self, args: &[&str], stdin_data: Option<&str>) -> std::process::Output {
        let mut cmd = self.cmd(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn().expect("spawn crew");
        if let Some(data) = stdin_data
            && let Some(mut stdin) = child.stdin.take()
        {
            let _ = stdin.write_all(data.as_bytes());
        }
        child.wait_with_output().expect("wait crew")
    }

    fn run_dir(&self, name: &str) -> PathBuf {
        self.crew_home.join("runs").join(name)
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Best-effort cleanup of any leftover supervisors in this test's CREW_HOME.
        let runs = self.crew_home.join("runs");
        if let Ok(entries) = fs::read_dir(&runs) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let _ = self.cmd(&["job", "stop", &name]).output();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn grok_ok_script(sleep_secs: &str, text: &str) -> String {
    format!(
        "sleep {sleep_secs}\n\
         echo '{{\"type\":\"session\",\"sessionId\":\"sess-1\"}}'\n\
         echo '{{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"{text}\"}}'"
    )
}

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

#[test]
fn concurrent_job_starts_never_fail_to_commit_run_json() {
    let env = TestEnv::new("concurrent-start");
    env.write_script("grok", &grok_ok_script("0.1", "ok"));

    let total = 24usize;
    let handles: Vec<_> = (0..total)
        .map(|i| {
            let crew_home = env.crew_home.clone();
            let path = env.path_env();
            std::thread::spawn(move || {
                let name = format!("j{i}");
                let mut child = Command::new(CREW_BIN)
                    .args(["job", "run", &name, "grok", "-"])
                    .env("CREW_HOME", &crew_home)
                    .env("PATH", &path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child.stdin.take().unwrap().write_all(b"ping").unwrap();
                let out = child.wait_with_output().unwrap();
                assert!(
                    out.status.success(),
                    "job {name} start failed: status={} stderr={}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
                name
            })
        })
        .collect();

    let names: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let mut wait_args = vec!["job", "wait"];
    for n in &names {
        wait_args.push(n);
    }
    let wait_out = env.run_with_stdin(&wait_args, None);
    assert!(
        wait_out.status.success(),
        "wait failed: stderr={}",
        String::from_utf8_lossy(&wait_out.stderr)
    );

    // Racing two starts on the exact same job name must allow one and reject the other cleanly.
    let r1 = env
        .cmd(&["job", "run", "dup", "grok", "ping"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let r2 = env
        .cmd(&["job", "run", "dup", "grok", "ping"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let o1 = r1.wait_with_output().unwrap();
    let o2 = r2.wait_with_output().unwrap();
    assert_ne!(o1.status.success(), o2.status.success());
    let wait_dup = env.run_with_stdin(&["job", "wait", "dup"], None);
    assert!(wait_dup.status.success());
    assert!(env.run_dir("dup").join("run.json").is_file());
    let _: PathBuf = Path::new(&env.crew_home).to_path_buf();
}
