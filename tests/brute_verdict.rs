//! Verdict preservation: after a judge writes VERDICT: FAIL, the next worker
//! invocation must see it.

mod common;
use common::*;

#[test]
fn brute_verdict_preserved_across_iterations() {
    let p = Project::new();
    seed_loop(&p, "allow .\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(
        &["run", "--no-sandbox"],
        &[
            ("PATH", &path),
            ("MOCK_JUDGE", "fail-first"),
            ("MOCK_WITNESS", "1"),
        ],
    );
    ok!(&out, "brute run");

    let argv = p.read(".loop/.omp-argv");
    has!(&argv, "--mode\nrpc", "worker argv");
    has!(&argv, "--auto-approve", "worker argv");
    has!(&argv, "--mode\njson", "judge argv");
    has!(&argv, "-p\nRead .loop/judge.md", "judge argv");

    assert!(
        p.read(".loop/.witness-1").is_empty(),
        "first worker run should see an empty verdict"
    );
    has!(
        &p.read(".loop/.witness-2"),
        "VERDICT: FAIL",
        "retry witness"
    );
}
