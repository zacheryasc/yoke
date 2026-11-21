//! Behavioral test: pause freezes the live RPC worker at a block boundary,
//! resume reuses the same process, queued steering survives the pause, and
//! edits made to protected files while paused become the new restore point.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail

for arg in "$@"; do
    if [[ "$arg" == *"judge.md"* ]]; then
        printf 'VERDICT: PASS\n\npause judge okay\n' > .loop/verdict.md
        exit 0
    fi
done

if [[ "$*" != *"--mode rpc"* ]]; then
    exit 0
fi

echo '{"type":"ready"}'
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        cat <<'JSON'
{"type":"session","version":3,"id":"sess-pause","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
JSON
        echo "$$" > .loop/worker.pid
        touch .loop/block-one
        sleep 0.4
        echo '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block one"}}'
        sleep 0.4
        echo '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block two"}}'
    elif [[ "$line" == *'"type":"steer"'* ]]; then
        echo "$line" >> .loop/steer-requests
        printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
        printf 'STATUS: DONE\n\nPaused steering handled.\n' > .loop/notes.md
        echo '{"type":"agent_end","messages":[]}'
        exit 0
    fi
done
"#;

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("condition not met within {:?}", timeout);
}

fn wait_until_logged(timeout: Duration, log_path: &std::path::Path, needle: &str) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let log = fs::read_to_string(log_path).unwrap_or_default();
        if log.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let log = fs::read_to_string(log_path).unwrap_or_default();
    panic!(
        "log did not contain {:?} within {:?}\nlog:\n{}",
        needle, timeout, log
    );
}

#[test]
fn pause_freezes_worker_and_resume_preserves_paused_edits() {
    let yoke = std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    let init = Command::new(&yoke)
        .args(["init"])
        .current_dir(project)
        .output()
        .expect("yoke init");
    assert!(init.status.success(), "yoke init failed");

    fs::write(
        project.join(".loop/yoke.conf"),
        "metrics off\nallow .\nsummary-every 0\n",
    )
    .unwrap();

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    let mock_omp = mock_bin_dir.join("omp");
    fs::write(&mock_omp, MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp, fs::Permissions::from_mode(0o755)).unwrap();
    let test_path = format!(
        "{}:{}",
        mock_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let log_path = project.join(".loop/run.log");
    let log_file = fs::File::create(&log_path).unwrap();
    let mut child = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn yoke run");

    wait_until_logged(Duration::from_secs(10), &log_path, "Launching OMP RPC");
    wait_until(Duration::from_secs(10), || {
        project.join(".loop/block-one").exists()
    });
    let worker_pid: i32 = fs::read_to_string(project.join(".loop/worker.pid"))
        .expect("worker pid")
        .trim()
        .parse()
        .expect("pid");

    let pause_started = Instant::now();
    let pause = Command::new(&yoke)
        .args(["pause"])
        .current_dir(project)
        .output()
        .expect("yoke pause");
    assert!(
        pause.status.success(),
        "pause failed: {}",
        String::from_utf8_lossy(&pause.stderr)
    );

    wait_until_logged(
        Duration::from_secs(10),
        &log_path,
        "worker paused at block boundary",
    );
    assert!(
        pause_started.elapsed() >= Duration::from_millis(250),
        "pause must wait for the open text block to end, elapsed {:?}",
        pause_started.elapsed()
    );
    let state = fs::read_to_string(format!("/proc/{worker_pid}/stat")).expect("worker stat");
    let state = state
        .split_whitespace()
        .nth(2)
        .expect("process state field");
    assert_eq!(state, "T", "worker process should be stopped");

    // Paused edits to protected files must survive the next restore.
    fs::write(project.join(".loop/plan.md"), "## Stage 1\nPAUSED EDIT\n").unwrap();

    let inject = Command::new(&yoke)
        .args(["inject", "finish after the paused edit"])
        .current_dir(project)
        .output()
        .expect("yoke inject");
    assert!(
        inject.status.success(),
        "pause-time inject should be accepted: {}",
        String::from_utf8_lossy(&inject.stderr)
    );

    let resume = Command::new(&yoke)
        .args(["resume"])
        .current_dir(project)
        .output()
        .expect("yoke resume");
    assert!(
        resume.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resume.stderr)
    );

    let status = child.wait().expect("wait yoke run");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("worker resumed"),
        "resume should be logged:\n{}",
        log
    );

    let logged_pid: String = fs::read_to_string(project.join(".loop/worker.pid"))
        .unwrap_or_default()
        .trim()
        .to_string();
    assert_eq!(
        logged_pid,
        worker_pid.to_string(),
        "resume must not restart the RPC worker"
    );

    let steer = fs::read_to_string(project.join(".loop/steer-requests")).expect("steer requests");
    assert!(
        steer.contains("finish after the paused edit"),
        "queued steering should deliver after resume:\n{}",
        steer
    );

    let plan = fs::read_to_string(project.join(".loop/plan.md")).expect("plan");
    assert!(
        plan.contains("PAUSED EDIT"),
        "paused plan edit was clobbered by the iteration restore:\n{}",
        plan
    );
}
