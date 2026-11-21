//! A brute run captures a self-contained artifact directory under
//! `.loop/runs/<run-id>/` (per-iteration diffs, git branch/SHA) and never
//! commits the worker's change.

mod common;
use common::*;
use std::fs;

#[test]
fn run_captures_patch_and_vcs_metadata_without_committing() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\n");
    let branch = git(p.path(), &["symbolic-ref", "--short", "HEAD"]);
    let base_sha = git(p.path(), &["rev-parse", "HEAD"]);

    let path = install_mock(&p, MOCK);
    let out = p.run_env(
        &["run", "--no-sandbox"],
        &[("PATH", &path), ("MOCK_WORKER", "patch")],
    );
    ok!(&out, "yoke run");

    let run_dirs: Vec<_> = fs::read_dir(p.path_of(".loop/runs"))
        .expect("runs dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(run_dirs.len(), 1, "expected one run dir: {run_dirs:?}");
    let run_dir = &run_dirs[0];

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(manifest["outcome"], "judge_pass");
    assert_eq!(manifest["iterations"], 1);
    assert_eq!(manifest["vcs"]["kind"], "git");
    assert_eq!(manifest["vcs"]["base_branch"], branch);
    assert_eq!(manifest["vcs"]["base_sha"], base_sha);
    for rel in [
        "summary.md",
        "snapshots/yoke.conf",
        "snapshots/plan.md",
        "iterations/0001/guard-results.md",
        "iterations/0001/notes.md",
    ] {
        assert!(
            run_dir.join(rel).is_file(),
            "{rel} should be captured in the run artifacts"
        );
    }

    let iter_meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("iterations/0001/meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(iter_meta["iteration"], 1);
    assert_eq!(iter_meta["guards_passed"], true);
    assert_eq!(iter_meta["status_done"], true);
    assert_eq!(iter_meta["vcs"]["base_sha"], base_sha);

    let patch = fs::read_to_string(run_dir.join("iterations/0001/diff.patch")).unwrap();
    assert!(
        patch.contains("feature.txt") && patch.contains("+artifact smoke"),
        "iteration patch:\n{patch}"
    );
    has!(&patch, "feature.txt", "iteration patch");
    lacks!(&patch, ".loop/", "iteration patch");

    let final_patch = fs::read_to_string(run_dir.join("final.diff")).unwrap();
    has!(&final_patch, "feature.txt", "final diff");

    assert_eq!(
        git(p.path(), &["rev-list", "--count", "HEAD"]),
        "1",
        "yoke must not commit the worker's change"
    );
    has!(
        &git(p.path(), &["status", "--porcelain"]),
        "?? feature.txt",
        "uncommitted worker change"
    );

    ok!(&p.run(&["clean"]), "yoke clean");
    assert!(
        run_dir.join("run.json").is_file() && run_dir.join("final.diff").is_file(),
        "run artifacts should survive `yoke clean`"
    );
}
