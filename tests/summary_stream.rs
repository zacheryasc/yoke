//! Behavioral test: a run with `--summary-fd` emits summary records on the
//! dedicated descriptor only. Summaries are block-driven and use the
//! configured cheap model, not the worker invocation.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

unsafe extern "C" {
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
}

const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail

for arg in "$@"; do
    if [[ "$arg" == *"judge.md"* ]]; then
        printf 'VERDICT: PASS\n\nsummary judge okay\n' > .loop/verdict.md
        exit 0
    fi
done

if [[ "$*" == *"--mode text"* ]]; then
    if [[ "$*" != *"--model fake-cheap"* ]]; then
        echo "summarizer used wrong model" >&2
        exit 9
    fi
    for required in --no-session --no-tools --no-extensions --no-skills --no-rules --no-context-files; do
        if [[ "$*" != *"$required"* ]]; then
            echo "summarizer missing $required" >&2
            exit 8
        fi
    done
    echo "cheap summary of the batch"
    exit 0
fi

if [[ "$*" != *"--mode rpc"* ]]; then
    exit 0
fi

echo '{"type":"ready"}'
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf 'STATUS: DONE\n\nSummary smoke complete.\n' > .loop/notes.md
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        cat <<'JSON'
{"type":"session","version":3,"id":"sess-summary","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"first block"}}
{"type":"message_update","assistantMessageEvent":{"type":"thinking_end","contentIndex":0,"content":"second block"}}
{"type":"tool_execution_end","toolName":"bash","toolCallId":"t1","isError":false}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"partial block"}}
{"type":"agent_end","messages":[]}
JSON
        exit 0
    fi
done
"#;

const SLOW_SUMMARY_MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
for arg in "$@"; do
    if [[ "$arg" == *"judge.md"* ]]; then
        printf 'VERDICT: PASS\n\nbackpressure judge okay\n' > .loop/verdict.md
        exit 0
    fi
done
if [[ "$*" == *"--mode text"* ]]; then
    sleep 0.5
    echo "slow summary"
    exit 0
fi
if [[ "$*" != *"--mode rpc"* ]]; then
    exit 0
fi
echo '{"type":"ready"}'
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf 'STATUS: DONE\n\nBackpressure smoke complete.\n' > .loop/notes.md
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        for n in 1 2 3 4 5 6; do
            printf '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block %s"}}\n' "$n"
            sleep 0.15
        done
        echo '{"type":"agent_end","messages":[]}'
        exit 0
    fi
done
"#;

#[test]
fn summaries_arrive_on_the_side_stream_in_block_batches() {
    let yoke = std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"));
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("seed.txt"), "seed\n").unwrap();
    let init = Command::new(&yoke)
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("yoke init");
    assert!(init.status.success(), "yoke init failed");

    fs::write(
        tmp.path().join(".loop/yoke.conf"),
        "metrics off\nallow .\nsummary-every 2\nsummary-model fake-cheap\nsummary-thinking off\nsummary-queue-limit 4\n",
    )
    .unwrap();

    let mock_bin_dir = tmp.path().join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    let mock_omp = mock_bin_dir.join("omp");
    fs::write(&mock_omp, MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp, fs::Permissions::from_mode(0o755)).unwrap();
    let test_path = format!(
        "{}:{}",
        mock_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let (child_end, parent_end) = UnixStream::pair().expect("socket pair");
    let mut command = Command::new(&yoke);
    command
        .args(["run", "--no-sandbox", "--summary-fd", "3"])
        .current_dir(tmp.path())
        .env("PATH", &test_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let summary_fd = child_end.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if dup2(summary_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // UnixStream pair fds start close-on-exec; dup2(3, 3) does not
            // clear that, so the exec would drop the summary stream.
            if fcntl(3, 2, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().expect("spawn yoke run");
    drop(child_end);
    let stderr = child.stderr.take().expect("stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        use std::io::Read;
        let mut stderr = stderr;
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let records: Vec<serde_json::Value> = BufReader::new(parent_end)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(&line).expect("summary record JSON"))
        .collect();

    let status = child.wait().expect("wait yoke run");
    let stderr = stderr_reader.join().expect("stderr reader");
    assert!(status.success(), "run should pass\nstderr:\n{}", stderr);

    let start = records
        .iter()
        .find(|record| record.get("type").and_then(|v| v.as_str()) == Some("run_start"))
        .expect("run_start record");
    assert!(
        start.get("run_id").and_then(|v| v.as_str()).is_some(),
        "run_start carries run id: {start}"
    );

    let summaries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| record.get("type").and_then(|v| v.as_str()) == Some("summary"))
        .collect();
    assert!(
        !summaries.is_empty(),
        "at least one summary should arrive: {records:?}"
    );
    for summary in &summaries {
        assert_eq!(
            summary.get("text").and_then(|v| v.as_str()),
            Some("cheap summary of the batch"),
            "summary text comes from the cheap model: {summary}"
        );
    }
    assert!(
        summaries
            .iter()
            .any(|summary| summary.get("blocks").and_then(|v| v.as_u64()) == Some(2)),
        "the first batch is exactly summary-every blocks: {summaries:?}"
    );

    let end = records
        .iter()
        .find(|record| record.get("type").and_then(|v| v.as_str()) == Some("run_end"))
        .expect("run_end record");
    assert_eq!(
        end.get("outcome").and_then(|v| v.as_str()),
        Some("judge_pass")
    );

    // Artifacts never carry the summary stream.
    let runs_dir = tmp.path().join(".loop/runs");
    let run_dir = fs::read_dir(&runs_dir)
        .expect("runs dir")
        .next()
        .expect("one run")
        .expect("run entry")
        .path();
    assert!(
        !run_dir.join("summaries.ndjson").exists(),
        "summaries must stay out of run artifacts"
    );
}

#[test]
fn slow_summaries_pause_the_worker_and_resume_when_drained() {
    let yoke = std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let init = Command::new(&yoke)
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("yoke init");
    assert!(init.status.success(), "yoke init failed");

    fs::write(
        tmp.path().join(".loop/yoke.conf"),
        "metrics off\nallow .\nsummary-every 1\nsummary-model fake-cheap\nsummary-queue-limit 1\n",
    )
    .unwrap();

    let mock_bin_dir = tmp.path().join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    let mock_omp = mock_bin_dir.join("omp");
    fs::write(&mock_omp, SLOW_SUMMARY_MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp, fs::Permissions::from_mode(0o755)).unwrap();
    let test_path = format!(
        "{}:{}",
        mock_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let log_path = tmp.path().join(".loop/run.log");
    let log_file = fs::File::create(&log_path).unwrap();
    let (child_end, parent_end) = UnixStream::pair().expect("socket pair");
    let mut command = Command::new(&yoke);
    command
        .args(["run", "--no-sandbox", "--summary-fd", "3"])
        .current_dir(tmp.path())
        .env("PATH", &test_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    let summary_fd = child_end.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if dup2(summary_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if fcntl(3, 2, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().expect("spawn yoke run");
    drop(child_end);
    let records: Vec<serde_json::Value> = BufReader::new(parent_end)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(&line).expect("summary record JSON"))
        .collect();
    let status = child.wait().expect("wait yoke run");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(status.success(), "run should pass\nlog:\n{}", log);
    assert!(
        log.contains("summary backlog reached"),
        "backpressure should be logged:\n{}",
        log
    );
    assert!(
        log.contains("worker paused at block boundary"),
        "worker should pause at a block boundary:\n{}",
        log
    );
    assert!(
        log.contains("worker resumed"),
        "worker should resume after draining:\n{}",
        log
    );

    let summaries = records
        .iter()
        .filter(|record| record.get("type").and_then(|v| v.as_str()) == Some("summary"))
        .count();
    assert!(
        summaries >= 6,
        "no summarizer work should be dropped, got {summaries} records: {records:?}"
    );
}
