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
fn judge_options_override_worker_and_periodic_defaults() {
    for (settings, models, thinking) in [
        ("", [None; 3], [None; 3]),
        (
            "model worker\nthinking xhigh\n",
            [Some("worker"); 3],
            [Some("xhigh"); 3],
        ),
        (
            "model worker\nthinking xhigh\njudge-thinking auto\n",
            [Some("worker"); 3],
            [Some("xhigh"), Some("xhigh"), Some("auto")],
        ),
        (
            "model worker\nthinking high\njudge-model provider/judge\njudge-thinking low\n",
            [Some("worker"), Some("worker"), Some("provider/judge")],
            [Some("high"), Some("high"), Some("low")],
        ),
        (
            "judge-model provider/judge\n",
            [None, None, Some("provider/judge")],
            [None; 3],
        ),
    ] {
        let p = Project::new();
        seed_loop(
            &p,
            &format!("metrics off\nallow .\nperiodic .loop/reviewer.md 1\n{settings}"),
        );
        p.write(".loop/reviewer.md", "Review the work.\n");
        let path = install_mock(&p, MOCK);
        let out = p.run_env(&["run"], &[("PATH", &path)]);
        ok!(&out, "yoke run");

        let argv = p.read(".loop/.omp-argv");
        let (worker, rest) = argv
            .strip_prefix("CALL\n")
            .unwrap()
            .split_once("\nCALL\n")
            .unwrap();
        let (periodic, judge) = rest.split_once("\nCALL\n").unwrap();
        has!(worker, "--mode\nrpc", "worker argv");
        has!(periodic, "Read .loop/reviewer.md", "periodic argv");
        has!(judge, "Read .loop/judge.md.", "judge argv");
        let calls = [worker, periodic, judge];
        assert_eq!(
            calls.map(|call| call.lines().skip_while(|arg| *arg != "--model").nth(1)),
            models,
            "model arguments: {argv}"
        );
        assert_eq!(
            calls.map(|call| call.lines().skip_while(|arg| *arg != "--thinking").nth(1)),
            thinking,
            "thinking arguments: {argv}"
        );
    }
}
