use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn yoke_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("parent of deps dir")
        .to_path_buf();
    path.push("yoke");
    path
}

fn build_yoke() -> std::path::PathBuf {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");
    yoke_bin()
}

fn git_init(project: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(project)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()
            .unwrap_or_else(|e| panic!("git {:?} failed: {}", args, e));
        assert!(
            out.status.success(),
            "git {:?} failed\nstdout={}\nstderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init"]);
    fs::write(project.join("seed.txt"), "seed\n").unwrap();
    git(&["add", "seed.txt"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@test",
        "commit",
        "-m",
        "seed",
    ]);
}

fn seed_loop(project: &std::path::Path) {
    git_init(project);
    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();
    fs::write(
        loop_dir.join("protocol.md"),
        "# Protocol\nWait for steering.\n",
    )
    .unwrap();
    fs::write(loop_dir.join("plan.md"), "## Stage 1\nSteer.\n").unwrap();
    fs::write(loop_dir.join("yoke.conf"), "metrics off\nallow .\n").unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();
}

fn install_mock(project: &std::path::Path, script: &str) -> String {
    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    let mock_omp = mock_bin_dir.join("omp");
    fs::write(&mock_omp, script).unwrap();
    fs::set_permissions(&mock_omp, fs::Permissions::from_mode(0o755)).unwrap();
    let original_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", mock_bin_dir.display(), original_path)
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("condition not met within {:?}", timeout);
}

fn wait_output(mut child: std::process::Child, timeout: Duration) -> Output {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("wait_with_output");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("wait_with_output after kill");
    panic!(
        "process timed out\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const STEERABLE_MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
MODE=""
for arg in "$@"; do
    if [[ "$arg" == "rpc" ]]; then MODE="rpc"; fi
done
if [[ "$MODE" != "rpc" ]]; then
    exit 0
fi

echo '{"type":"ready"}'
while IFS= read -r line; do
    echo "$line" >> .loop/.rpc-requests
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        cat <<'JSON'
{"type":"session","version":3,"id":"sess-steerable","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"waiting"}}
JSON
    elif [[ "$line" == *'"type":"steer"'* ]]; then
        echo "$line" >> .loop/.steer-requests
        printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
        printf 'STATUS: DONE\n\nSteering handled.\n' > .loop/notes.md
        cat <<'JSON'
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"steered"}}
{"type":"agent_end","messages":[]}
JSON
        exit 0
    elif [[ "$line" == *'"type":"abort_and_prompt"'* ]]; then
        echo "$line" >> .loop/.steer-requests
        printf '{"id":"%s","type":"response","command":"abort_and_prompt","success":true}\n' "$id"
        printf 'STATUS: DONE\n\nAbort steering handled.\n' > .loop/notes.md
        echo '{"type":"agent_end","messages":[]}'
        exit 0
    fi
done
"#;

const DONE_MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
MODE=""
for arg in "$@"; do
    if [[ "$arg" == "rpc" ]]; then MODE="rpc"; fi
done
if [[ "$MODE" == "rpc" ]]; then
    echo '{"type":"ready"}'
    while IFS= read -r line; do
        id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
        if [[ "$line" == *'"type":"prompt"'* ]]; then
            printf 'STATUS: DONE\n' > .loop/notes.md
            printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
            cat <<'JSON'
{"type":"session","version":3,"id":"sess-done","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"agent_end","messages":[]}
JSON
            exit 0
        fi
    done
fi
"#;

const REJECT_STEER_MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" != *"--mode rpc"* ]]; then
    exit 0
fi
echo '{"type":"ready"}'
while IFS= read -r line; do
    echo "$line" >> .loop/.rpc-requests
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        cat <<'JSON'
{"type":"session","version":3,"id":"sess-reject","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"waiting"}}
JSON
    elif [[ "$line" == *'"type":"steer"'* ]]; then
        echo "$line" >> .loop/.steer-requests
        printf '{"id":"%s","type":"response","command":"steer","success":false,"error":"steering rejected by fake RPC"}\n' "$id"
        echo '{"type":"agent_end","messages":[]}'
        exit 0
    fi
done
"#;

#[test]
fn inject_without_active_run_fails_clearly() {
    let yoke = build_yoke();
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(&yoke)
        .args(["inject", "hello"])
        .current_dir(tmp.path())
        .output()
        .expect("yoke inject");

    assert!(
        !output.status.success(),
        "inject should fail without an active run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no active yoke run"),
        "stderr should describe missing active run, got:\n{}",
        stderr
    );
}

#[test]
fn empty_injection_fails_before_socket_lookup() {
    let yoke = build_yoke();
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(&yoke)
        .args(["inject", ""])
        .current_dir(tmp.path())
        .output()
        .expect("yoke inject empty");

    assert_eq!(
        output.status.code(),
        Some(2),
        "empty message is usage error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("inject message must not be empty"),
        "stderr should describe empty message, got:\n{}",
        stderr
    );
}

#[test]
fn normal_injection_reaches_active_rpc_worker() {
    let yoke = build_yoke();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_loop(project);
    let test_path = install_mock(project, STEERABLE_MOCK_OMP);
    let loop_dir = project.join(".loop");

    let child = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn yoke run");

    wait_until(Duration::from_secs(5), || {
        loop_dir.join(".rpc-requests").exists()
    });

    let inject = Command::new(&yoke)
        .args(["inject", "focus on the failing guard"])
        .current_dir(project)
        .output()
        .expect("yoke inject");
    assert!(
        inject.status.success(),
        "inject should be accepted\nstderr={}",
        String::from_utf8_lossy(&inject.stderr)
    );

    let output = wait_output(child, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "run should complete after mock handles steering\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let steer = fs::read_to_string(loop_dir.join(".steer-requests")).expect("steer request");
    assert!(
        steer.contains("YOKE HUMAN STEERING"),
        "steer request:\n{}",
        steer
    );
    assert!(
        steer.contains("focus on the failing guard"),
        "steer request:\n{}",
        steer
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("accepted normal steering"),
        "stderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("delivered normal steering"),
        "stderr:\n{}",
        stderr
    );
}

#[test]
fn shutdown_injection_requests_graceful_wind_down_and_stops_run() {
    let yoke = build_yoke();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_loop(project);
    let test_path = install_mock(project, STEERABLE_MOCK_OMP);
    let loop_dir = project.join(".loop");

    let child = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn yoke run");

    wait_until(Duration::from_secs(5), || {
        loop_dir.join(".rpc-requests").exists()
    });

    let inject = Command::new(&yoke)
        .args(["inject", "--shutdown", "time to stop"])
        .current_dir(project)
        .output()
        .expect("yoke inject shutdown");
    assert!(
        inject.status.success(),
        "shutdown inject should be accepted\nstderr={}",
        String::from_utf8_lossy(&inject.stderr)
    );

    let output = wait_output(child, Duration::from_secs(5));
    assert_eq!(
        output.status.code(),
        Some(130),
        "shutdown steering should stop the run as an interrupt\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let steer = fs::read_to_string(loop_dir.join(".steer-requests")).expect("steer request");
    assert!(
        steer.contains("YOKE SHUTDOWN REQUEST"),
        "steer request:\n{}",
        steer
    );
    assert!(
        steer.contains("Stop starting new substantial work"),
        "steer request:\n{}",
        steer
    );
    assert!(steer.contains("exit promptly"), "steer request:\n{}", steer);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Shutdown steering handled"),
        "stderr:\n{}",
        stderr
    );
}

#[test]
fn delivery_failure_is_visible_to_inject_and_run() {
    let yoke = build_yoke();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_loop(project);
    let test_path = install_mock(project, REJECT_STEER_MOCK_OMP);
    let loop_dir = project.join(".loop");

    let child = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn yoke run");

    wait_until(Duration::from_secs(5), || {
        loop_dir.join(".rpc-requests").exists()
    });

    let inject = Command::new(&yoke)
        .args(["inject", "this should fail"])
        .current_dir(project)
        .output()
        .expect("yoke inject failure");
    assert!(
        !inject.status.success(),
        "inject should fail when OMP RPC rejects steering"
    );
    let inject_stderr = String::from_utf8_lossy(&inject.stderr);
    assert!(
        inject_stderr.contains("steering rejected by fake RPC"),
        "inject stderr should surface delivery failure, got:\n{}",
        inject_stderr
    );

    let output = wait_output(child, Duration::from_secs(5));
    assert!(
        !output.status.success(),
        "run should fail after OMP RPC rejects steering"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("OMP RPC rejected normal steering"),
        "run stderr should surface rejected steering, got:\n{}",
        stderr
    );
}

#[test]
fn run_replaces_stale_socket_and_refuses_live_socket() {
    let yoke = build_yoke();

    let stale_tmp = tempfile::tempdir().expect("stale tempdir");
    let stale_project = stale_tmp.path();
    seed_loop(stale_project);
    let stale_path = install_mock(stale_project, DONE_MOCK_OMP);
    let stale_socket = stale_project.join(".loop/yoke.sock");
    let stale_listener = UnixListener::bind(&stale_socket).expect("bind stale socket");
    drop(stale_listener);
    assert!(stale_socket.exists(), "stale socket should remain on disk");

    let stale_output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(stale_project)
        .env("PATH", &stale_path)
        .output()
        .expect("yoke run with stale socket");
    assert!(
        stale_output.status.success(),
        "run should replace stale socket\nstderr={}",
        String::from_utf8_lossy(&stale_output.stderr)
    );
    assert!(
        !stale_socket.exists(),
        "socket should be removed after normal run exit"
    );

    let live_tmp = tempfile::tempdir().expect("live tempdir");
    let live_project = live_tmp.path();
    seed_loop(live_project);
    let live_path = install_mock(live_project, DONE_MOCK_OMP);
    let live_socket = live_project.join(".loop/yoke.sock");
    let _live_listener = UnixListener::bind(&live_socket).expect("bind live socket");
    let live_output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(live_project)
        .env("PATH", &live_path)
        .output()
        .expect("yoke run with live socket");
    assert!(
        !live_output.status.success(),
        "run should refuse live socket ownership"
    );
    let stderr = String::from_utf8_lossy(&live_output.stderr);
    assert!(
        stderr.contains("another active yoke run already owns"),
        "stderr should describe live socket conflict, got:\n{}",
        stderr
    );
    drop(UnixStream::connect(&live_socket));
}
