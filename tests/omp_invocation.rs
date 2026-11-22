//! The default backend invokes `omp` in RPC mode with model/thinking flags and
//! sends the worker prompt over RPC stdin. It must not fall back to the legacy
//! `claude` binary.

mod common;
use common::*;

#[test]
fn default_backend_invokes_omp_with_model_and_thinking() {
    let p = Project::new();
    seed_loop(&p, "model gpt-5.5\nthinking xhigh\nallow .\n");
    let path = install_mock(&p, MOCK);
    install_mock_exe(
        &p,
        "claude",
        "#!/usr/bin/env bash\necho 'legacy agent must not be invoked' >&2\nexit 99\n",
    );

    let out = p.run_env(&["run"], &[("PATH", &path)]);
    ok!(&out, "yoke run");

    let argv = p.read(".loop/.omp-argv");
    has!(&argv, "--mode\nrpc", "worker argv");
    has!(&argv, "--auto-approve", "worker argv");
    has!(&argv, "--model\ngpt-5.5", "worker argv");
    has!(&argv, "--thinking\nxhigh", "worker argv");

    let stdin = p.read(".loop/.omp-stdin");
    has!(&stdin, "\"type\":\"prompt\"", "rpc stdin");
    has!(
        &stdin,
        "Read .loop/protocol.md and follow its instructions.",
        "rpc stdin"
    );
}

#[test]
fn judge_thinking_overrides_worker_thinking_only_for_judge() {
    for (settings, expected_worker, expected_judge) in [
        ("", None, None),
        ("thinking xhigh\n", Some("xhigh"), Some("xhigh")),
        (
            "thinking xhigh\njudge-thinking auto\n",
            Some("xhigh"),
            Some("auto"),
        ),
    ] {
        let p = Project::new();
        seed_loop(&p, &format!("metrics off\nallow .\n{settings}"));
        let path = install_mock(&p, MOCK);
        let out = p.run_env(&["run"], &[("PATH", &path)]);
        ok!(&out, "yoke run");

        let argv = p.read(".loop/.omp-argv");
        let (worker, judge) = argv
            .strip_prefix("CALL\n")
            .unwrap()
            .split_once("\nCALL\n")
            .unwrap();
        has!(worker, "--mode\nrpc", "worker argv");
        assert_eq!(
            worker.lines().skip_while(|arg| *arg != "--thinking").nth(1),
            expected_worker,
            "worker argv: {worker}"
        );
        has!(judge, "Read .loop/judge.md.", "judge argv");
        assert_eq!(
            judge.lines().skip_while(|arg| *arg != "--thinking").nth(1),
            expected_judge,
            "judge argv: {judge}"
        );
    }
}
