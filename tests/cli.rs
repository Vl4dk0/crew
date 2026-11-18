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
         echo '{{\"type\":\"result\",\"result\":\"{text}\"}}'"
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

#[test]
fn after_unknown_and_self_exit_2_and_create_nothing() {
    let env = TestEnv::new("after-invalid");
    env.write_script("grok", &grok_ok_script("0", "ok"));

    let unknown = env.run_with_stdin(
        &["job", "run", "b", "grok", "--after", "missing", "hi"],
        None,
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert!(!env.run_dir("b").exists());

    let self_ref = env.run_with_stdin(&["job", "run", "b", "grok", "--after", "b", "hi"], None);
    assert_eq!(self_ref.status.code(), Some(2));
    assert!(!env.run_dir("b").exists());
}

#[test]
fn after_chain_waits_for_queue_timeout_from_start_and_ignores_resume() {
    let env = TestEnv::new("after-chain");
    let marker = env.root.join("ran_b");
    let resume_marker = env.root.join("resume_ran");
    env.write_script(
        "grok",
        &format!(
            "echo '{{\"type\":\"session\",\"sessionId\":\"sess-1\"}}'\n\
             case \"$*\" in\n\
               *MARKER_B*)\n\
                 touch \"{}\"\n\
                 sleep 0.1\n\
                 echo '{{\"type\":\"result\",\"result\":\"from-b\"}}'\n\
                 ;;\n\
               *RESUME_FAIL*)\n\
                 touch \"{}\"\n\
                 exit 1\n\
                 ;;\n\
               *)\n\
                 sleep 0.35\n\
                 echo '{{\"type\":\"result\",\"result\":\"from-a\"}}'\n\
                 ;;\n\
             esac",
            marker.display(),
            resume_marker.display()
        ),
    );

    let out_a = env.run_with_stdin(&["job", "run", "a", "grok", "start"], None);
    assert!(out_a.status.success());

    // Queue a second turn onto `a` while `a` is still in turn 1.
    let q_a = env.run_with_stdin(&["job", "queue", "a", "second turn"], None);
    assert!(q_a.status.success());

    // `b` reads prompt from stdin and has `-t 0.6`; `a` takes ~0.7s across two turns,
    // so `b` would time out if `-t` started before `a` ended.
    let out_b = env.run_with_stdin(
        &[
            "job",
            "run",
            "b",
            "grok",
            "-C",
            env.root.to_str().unwrap(),
            "-t",
            "0.6",
            "--after",
            "a",
            "-",
        ],
        Some("MARKER_B"),
    );
    assert!(
        out_b.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    let out_c = env.run_with_stdin(&["job", "run", "c", "grok", "--after", "b", "final"], None);
    assert!(out_c.status.success());

    // `b` and `c` exist immediately as `waiting`, and `job queue`/`steer` reject them with exit 2.
    let show_b = env.run_with_stdin(&["job", "show", "b", "--json"], None);
    assert!(show_b.status.success());
    let json_b: serde_json::Value = serde_json::from_slice(&show_b.stdout).unwrap();
    assert_eq!(json_b["status"], "waiting");
    assert_eq!(json_b["after"], "a");
    assert!(!marker.exists());

    let ls_out = env.run_with_stdin(&["job", "ls"], None);
    let ls_text = String::from_utf8_lossy(&ls_out.stdout);
    assert!(ls_text.contains("waiting"), "ls:\n{ls_text}");

    let q_b = env.run_with_stdin(&["job", "queue", "b", "nope"], None);
    assert_eq!(q_b.status.code(), Some(2));
    let s_b = env.run_with_stdin(&["job", "steer", "b", "nope"], None);
    assert_eq!(s_b.status.code(), Some(2));

    // Wait for `a`'s session id to be recorded, then resume `a` into a failing `a.2`.
    // Waiter `b` must ignore `a.2` and wait only for `a` itself.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let show_a = env.run_with_stdin(&["job", "show", "a", "--json"], None);
        let v: serde_json::Value = serde_json::from_slice(&show_a.stdout).unwrap();
        if v["session_id"].as_str() == Some("sess-1") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session_id never appeared"
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    let res_a = env.run_with_stdin(&["job", "resume", "a", "RESUME_FAIL"], None);
    assert!(res_a.status.success());
    let wait_a2 = env.run_with_stdin(&["job", "wait", "a.2"], None);
    assert!(!wait_a2.status.success());
    assert!(resume_marker.exists());

    let wait_c = env.run_with_stdin(&["job", "wait", "c"], None);
    assert!(
        wait_c.status.success(),
        "c wait failed: code={:?} stderr={}",
        wait_c.status.code(),
        String::from_utf8_lossy(&wait_c.stderr)
    );
    assert!(marker.exists());
    let res_b = env.run_with_stdin(&["job", "result", "b"], None);
    assert_eq!(String::from_utf8_lossy(&res_b.stdout).trim(), "from-b");
}

#[test]
fn after_skips_cascade_across_failure_limit_timeout_stop_and_lost() {
    let env = TestEnv::new("after-skips");
    let never_ran = env.root.join("never_ran");
    env.write_script(
        "grok",
        &format!(
            "case \"$*\" in\n\
               *NEVER*)\n\
                 touch \"{}\"\n\
                 echo '{{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"oops\"}}'\n\
                 ;;\n\
               *FAIL*)\n\
                 exit 1\n\
                 ;;\n\
               *LIMIT*)\n\
                 echo '{{\"type\":\"error\",\"message\":\"usage limit reached\"}}'\n\
                 exit 1\n\
                 ;;\n\
               *SLEEP*)\n\
                 sleep 5\n\
                 ;;\n\
             esac",
            never_ran.display()
        ),
    );

    // 1. Failure + cascade + immediate decision for an already-ended predecessor.
    assert!(
        env.run_with_stdin(&["job", "run", "fa", "grok", "FAIL"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "fb", "grok", "--after", "fa", "NEVER"],
            None
        )
        .status
        .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "fc", "grok", "--after", "fb", "NEVER"],
            None
        )
        .status
        .success()
    );
    let w_fc = env.run_with_stdin(&["job", "wait", "fc"], None);
    assert!(!w_fc.status.success());
    let show_fb: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "fb", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_fb["status"], "skipped");
    let r_fb = show_fb["skip_reason"].as_str().unwrap();
    assert!(r_fb.contains("fa") && r_fb.contains("failed"), "{r_fb}");

    let show_fc: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "fc", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_fc["status"], "skipped");
    let r_fc = show_fc["skip_reason"].as_str().unwrap();
    assert!(r_fc.contains("fb") && r_fc.contains("skipped"), "{r_fc}");

    // Already-ended predecessor decides immediately.
    assert!(
        env.run_with_stdin(
            &["job", "run", "late", "grok", "--after", "fa", "NEVER"],
            None
        )
        .status
        .success()
    );
    let w_late = env.run_with_stdin(&["job", "wait", "late", "--timeout", "2"], None);
    assert_eq!(w_late.status.code(), Some(1));

    // 2. Limit (exit 75).
    assert!(
        env.run_with_stdin(&["job", "run", "la", "grok", "LIMIT"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "lb", "grok", "--after", "la", "NEVER"],
            None
        )
        .status
        .success()
    );
    assert_eq!(
        env.run_with_stdin(&["job", "wait", "lb"], None)
            .status
            .code(),
        Some(1)
    );
    let show_lb: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "lb", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_lb["status"], "skipped");
    let r_lb = show_lb["skip_reason"].as_str().unwrap();
    assert!(r_lb.contains("la") && r_lb.contains("limit"), "{r_lb}");

    // 3. Timeout (exit 124).
    assert!(
        env.run_with_stdin(&["job", "run", "ta", "grok", "-t", "0.2", "SLEEP"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "tb", "grok", "--after", "ta", "NEVER"],
            None
        )
        .status
        .success()
    );
    assert_eq!(
        env.run_with_stdin(&["job", "wait", "tb"], None)
            .status
            .code(),
        Some(1)
    );
    let show_tb: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "tb", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_tb["status"], "skipped");
    let r_tb = show_tb["skip_reason"].as_str().unwrap();
    assert!(r_tb.contains("ta") && r_tb.contains("timeout"), "{r_tb}");

    // 4. Stop predecessor -> skipped; stop waiting job -> stopped (never starts agent).
    assert!(
        env.run_with_stdin(&["job", "run", "sa", "grok", "SLEEP"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "sb", "grok", "--after", "sa", "NEVER"],
            None
        )
        .status
        .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "sw", "grok", "--after", "sa", "NEVER"],
            None
        )
        .status
        .success()
    );
    assert!(
        env.run_with_stdin(&["job", "stop", "sw"], None)
            .status
            .success()
    );
    assert_eq!(
        env.run_with_stdin(&["job", "wait", "sw"], None)
            .status
            .code(),
        Some(130)
    );
    let show_sw: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "sw", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_sw["status"], "stopped");

    assert!(
        env.run_with_stdin(&["job", "stop", "sa"], None)
            .status
            .success()
    );
    assert_eq!(
        env.run_with_stdin(&["job", "wait", "sb"], None)
            .status
            .code(),
        Some(1)
    );
    let show_sb: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "sb", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_sb["status"], "skipped");
    let r_sb = show_sb["skip_reason"].as_str().unwrap();
    assert!(r_sb.contains("sa") && r_sb.contains("stopped"), "{r_sb}");

    // 5. Lost predecessor (SIGKILL supervisor) -> detected and skipped without hanging.
    assert!(
        env.run_with_stdin(&["job", "run", "xa", "grok", "SLEEP"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &["job", "run", "xb", "grok", "--after", "xa", "NEVER"],
            None
        )
        .status
        .success()
    );
    let show_xa: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "xa", "--json"], None)
            .stdout,
    )
    .unwrap();
    let sup_pid = show_xa["supervisor_pid"].as_u64().unwrap() as i32;
    let wrap_pid = show_xa["wrapper_pid"].as_u64().map(|p| p as i32);
    unsafe {
        libc::kill(sup_pid, libc::SIGKILL);
        if let Some(wp) = wrap_pid {
            libc::killpg(wp, libc::SIGKILL);
        }
    }
    let w_xb = env.run_with_stdin(&["job", "wait", "xb", "--timeout", "3"], None);
    assert_eq!(w_xb.status.code(), Some(1));
    let show_xb: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "xb", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_xb["status"], "skipped");
    let r_xb = show_xb["skip_reason"].as_str().unwrap();
    assert!(r_xb.contains("xa") && r_xb.contains("lost"), "{r_xb}");

    assert!(
        !never_ran.exists(),
        "a skipped/stopped waiter started its agent"
    );
}

#[test]
fn grok_mid_run_usage_limit_exits_75_and_skips_after() {
    let env = TestEnv::new("grok-mid-limit");
    let never_ran = env.root.join("never_ran");
    let real_limit = "Error: You\u{2019}ve reached your free Grok Build usage limit for now. Get SuperGrok for much higher limits, or try again later: https://grok.com/supergrok?referrer=grok-build";
    env.write_script(
        "grok",
        &format!(
            "case \"$*\" in\n\
               *NEVER*)\n\
                 touch \"{}\"\n\
                 echo '{{\"type\":\"result\",\"result\":\"oops\"}}'\n\
                 ;;\n\
               *LIMIT_MID*)\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-mid\"}}'\n\
                 echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{{\"cmd\":\"ls\"}}}}]}}}}'\n\
                 echo '{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"ok\"}}]}}}}'\n\
                 echo '{{\"type\":\"result\",\"subtype\":\"success\",\"num_turns\":18,\"result\":\"\",\"stop_reason\":\"end_turn\",\"total_cost_usd\":0.941218,\"usage\":{{\"input_tokens\":100,\"output_tokens\":50}}}}'\n\
                 echo '{real_limit}' >&2\n\
                 exit 1\n\
                 ;;\n\
               *NORMAL_OK*)\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-ok\"}}'\n\
                 echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"The usage limit check lives in proc.rs.\"}}]}}}}'\n\
                 echo '{{\"type\":\"result\",\"subtype\":\"success\",\"num_turns\":2,\"result\":\"The usage limit check lives in proc.rs.\",\"stop_reason\":\"end_turn\"}}'\n\
                 ;;\n\
               *NORMAL_FAIL*)\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-fail\"}}'\n\
                 echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"The usage limit check lives in proc.rs.\"}}]}}}}'\n\
                 echo '{{\"type\":\"result\",\"subtype\":\"success\",\"num_turns\":2,\"result\":\"The usage limit check lives in proc.rs.\",\"stop_reason\":\"end_turn\"}}'\n\
                 echo 'Error: command failed' >&2\n\
                 exit 1\n\
                 ;;\n\
             esac",
            never_ran.display()
        ),
    );

    let direct = env.run_with_stdin(&["grok", "LIMIT_MID"], None);
    assert_eq!(direct.status.code(), Some(75));
    let direct_err = String::from_utf8_lossy(&direct.stderr);
    assert!(direct_err.contains("SESSION_ID=sess-mid"), "{direct_err}");
    assert!(
        direct_err.contains("[crew] tokens=150 turns=18 cost=$0.941218"),
        "{direct_err}"
    );
    assert!(
        direct_err.contains("[crew] grok usage limit:")
            && direct_err.contains("free Grok Build usage limit"),
        "{direct_err}"
    );

    assert!(
        env.run_with_stdin(&["job", "run", "limited", "grok", "LIMIT_MID"], None)
            .status
            .success()
    );
    assert!(
        env.run_with_stdin(
            &[
                "job",
                "run",
                "afterlimited",
                "grok",
                "--after",
                "limited",
                "NEVER"
            ],
            None
        )
        .status
        .success()
    );
    assert_eq!(
        env.run_with_stdin(&["job", "wait", "limited"], None)
            .status
            .code(),
        Some(75)
    );
    let show_text =
        String::from_utf8_lossy(&env.run_with_stdin(&["job", "show", "limited"], None).stdout)
            .into_owned();
    assert!(show_text.contains("status: failed"), "{show_text}");
    assert!(show_text.contains("exit_code: 75"), "{show_text}");

    assert_eq!(
        env.run_with_stdin(&["job", "wait", "afterlimited"], None)
            .status
            .code(),
        Some(1)
    );
    let show_after: serde_json::Value = serde_json::from_slice(
        &env.run_with_stdin(&["job", "show", "afterlimited", "--json"], None)
            .stdout,
    )
    .unwrap();
    assert_eq!(show_after["status"], "skipped");
    assert_eq!(show_after["skip_reason"], "limited failed (limit, exit 75)");
    assert!(!never_ran.exists(), "skipped job ran its agent");

    let ok = env.run_with_stdin(&["grok", "NORMAL_OK"], None);
    assert_eq!(ok.status.code(), Some(0));

    let fail = env.run_with_stdin(&["grok", "NORMAL_FAIL"], None);
    assert_eq!(fail.status.code(), Some(1));
    let fail_err = String::from_utf8_lossy(&fail.stderr);
    assert!(!fail_err.contains("usage limit:"), "{fail_err}");
}
