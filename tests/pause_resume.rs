//! Pause freezes the live RPC worker at a block boundary; resume reuses the
//! same process, queued steering is delivered, and edits to protected files
//! made while paused become the new restore point.

mod common;
use common::*;
use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[test]
fn pause_freezes_worker_and_resume_preserves_paused_edits() {
    let p = Project::new();
    ok!(&p.run(&["init"]), "yoke init");
    p.write(".loop/yoke.conf", "metrics off\nallow .\nsummary-every 0\n");
    let path = install_mock(&p, MOCK);

    let log_path = p.path_of(".loop/run.log");
    let log_file = fs::File::create(&log_path).unwrap();
    let mut child = p
        .cmd(
            &["run", "--no-sandbox"],
            &[("PATH", &path), ("MOCK_WORKER", "pause")],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn yoke run");

    wait_logged(Duration::from_secs(10), &log_path, "Launching OMP RPC");
    wait_until(Duration::from_secs(10), || p.exists(".loop/block-one"));
    let worker_pid: i32 = p.read(".loop/worker.pid").trim().parse().unwrap();

    let pause_started = Instant::now();
    ok!(&p.run(&["pause"]), "yoke pause");
    wait_logged(
        Duration::from_secs(10),
        &log_path,
        "worker paused at block boundary",
    );
    assert!(
        pause_started.elapsed() >= Duration::from_millis(250),
        "pause must wait for the open text block to end"
    );
    let stat = fs::read_to_string(format!("/proc/{worker_pid}/stat")).expect("worker stat");
    assert_eq!(
        stat.split_whitespace().nth(2),
        Some("T"),
        "worker process should be stopped"
    );

    // Paused edits to protected files must survive the next restore.
    p.write(".loop/plan.md", "## Stage 1\nPAUSED EDIT\n");
    ok!(
        &p.run(&["inject", "finish after the paused edit"]),
        "pause-time inject"
    );
    ok!(&p.run(&["resume"]), "yoke resume");

    let status = child.wait().expect("wait yoke run");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(status.success(), "run should pass\nlog:\n{log}");
    has!(&log, "worker resumed", "run log");
    assert_eq!(
        p.read(".loop/worker.pid").trim(),
        worker_pid.to_string(),
        "resume must not restart the RPC worker"
    );
    has!(
        &p.read(".loop/.steer-requests"),
        "finish after the paused edit",
        "queued steering"
    );
    has!(&p.read(".loop/plan.md"), "PAUSED EDIT", "paused plan edit");
}
