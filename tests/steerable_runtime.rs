//! Human steering: inject into a live RPC worker, graceful shutdown, stop,
//! delivery-failure visibility, and control-socket ownership.

mod common;
use common::*;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

fn start_run(p: &Project, mock_env: &[(&str, &str)]) -> std::process::Child {
    let path = install_mock(p, MOCK);
    let mut env = vec![("PATH", path.as_str())];
    env.extend_from_slice(mock_env);
    let child = p.spawn(&["run", "--no-sandbox"], &env);
    wait_until(Duration::from_secs(5), || p.exists(".loop/.omp-stdin"));
    child
}

#[test]
fn inject_without_active_run_fails_clearly() {
    let p = Project::new();
    let out = p.run(&["inject", "hello"]);
    failed!(&out, "inject without a run");
    has!(&stderr(&out), "no active yoke run", "inject stderr");
}

#[test]
fn empty_injection_fails_before_socket_lookup() {
    let p = Project::new();
    let out = p.run(&["inject", ""]);
    assert_eq!(out.status.code(), Some(2), "empty message is a usage error");
    has!(
        &stderr(&out),
        "inject message must not be empty",
        "inject stderr"
    );
}

#[test]
fn normal_injection_reaches_active_rpc_worker() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\n");
    let child = start_run(&p, &[("MOCK_WORKER", "await-steer")]);

    ok!(
        &p.run(&["inject", "focus on the failing guard"]),
        "yoke inject"
    );
    let out = wait_output(child, Duration::from_secs(5));
    ok!(&out, "run after steering");

    let steer = p.read(".loop/.steer-requests");
    has!(&steer, "YOKE HUMAN STEERING", "steer request");
    has!(&steer, "focus on the failing guard", "steer request");
    let err = stderr(&out);
    has!(&err, "accepted normal steering", "run stderr");
    has!(&err, "delivered normal steering", "run stderr");
}

#[test]
fn shutdown_injection_requests_graceful_wind_down_and_stops_run() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\n");
    let child = start_run(&p, &[("MOCK_WORKER", "await-steer")]);

    ok!(
        &p.run(&["inject", "--shutdown", "time to stop"]),
        "shutdown inject"
    );
    let out = wait_output(child, Duration::from_secs(5));
    assert_eq!(
        out.status.code(),
        Some(130),
        "shutdown steering stops the run as an interrupt:\n{}",
        stderr(&out)
    );
    let steer = p.read(".loop/.steer-requests");
    has!(&steer, "YOKE SHUTDOWN REQUEST", "steer request");
    has!(
        &steer,
        "Stop starting new substantial work",
        "steer request"
    );
    has!(&steer, "exit promptly", "steer request");
    has!(&stderr(&out), "Shutdown steering handled", "run stderr");
}

#[test]
fn stop_command_gracefully_ends_active_run() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\n");
    let child = start_run(&p, &[("MOCK_WORKER", "await-steer")]);

    ok!(&p.run(&["stop"]), "yoke stop");
    let out = wait_output(child, Duration::from_secs(5));
    assert_eq!(
        out.status.code(),
        Some(130),
        "graceful stop ends the run as an interrupt:\n{}",
        stderr(&out)
    );
    has!(
        &p.read(".loop/.steer-requests"),
        "YOKE SHUTDOWN REQUEST",
        "stop steering"
    );
}

#[test]
fn delivery_failure_is_visible_to_inject_and_run() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\n");
    let child = start_run(
        &p,
        &[("MOCK_WORKER", "await-steer"), ("MOCK_STEER", "reject")],
    );

    let out = p.run(&["inject", "this should fail"]);
    failed!(&out, "inject");
    has!(
        &stderr(&out),
        "steering rejected by fake RPC",
        "inject stderr"
    );

    let out = wait_output(child, Duration::from_secs(5));
    failed!(&out, "run after rejected steering");
    has!(
        &stderr(&out),
        "OMP RPC rejected normal steering",
        "run stderr"
    );
}

#[test]
fn run_replaces_stale_socket_and_refuses_live_socket() {
    let stale = Project::new();
    seed_loop(&stale, "metrics off\nallow .\n");
    let path = install_mock(&stale, MOCK);
    let stale_socket = stale.path_of(".loop/yoke.sock");
    drop(UnixListener::bind(&stale_socket).expect("bind stale socket"));
    assert!(stale_socket.exists(), "stale socket should remain on disk");

    let out = stale.run_env(&["run", "--no-sandbox"], &[("PATH", &path)]);
    ok!(&out, "run with stale socket");
    assert!(!stale_socket.exists(), "socket should be removed on exit");

    let live = Project::new();
    seed_loop(&live, "metrics off\nallow .\n");
    let path = install_mock(&live, MOCK);
    let live_socket = live.path_of(".loop/yoke.sock");
    let _listener = UnixListener::bind(&live_socket).expect("bind live socket");
    let out = live.run_env(&["run", "--no-sandbox"], &[("PATH", &path)]);
    failed!(&out, "run with live socket");
    has!(
        &stderr(&out),
        "another active yoke run already owns",
        "run stderr"
    );
    drop(UnixStream::connect(&live_socket));
}
