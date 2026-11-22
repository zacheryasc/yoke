//! Adversarial coverage for stash round-trips, plan-loop completion, brute
//! retry/bailout, judge-every on DONE, and guard-results writing.

mod common;
use common::*;
use std::fs;

fn stash_dir(p: &Project) -> std::path::PathBuf {
    let dirs: Vec<_> = fs::read_dir(p.home().join(".yoke/stash"))
        .expect("stash root")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "one project slug dir: {dirs:?}");
    dirs.into_iter().next().unwrap()
}

#[test]
fn stash_roundtrip_after_extraction() {
    let p = Project::new();
    ok!(&p.run(&["init"]), "yoke init");
    p.write(
        ".loop/plan.md",
        "## Stage 1 — Build the widget\n\nDo the thing.\n",
    );
    p.write(
        ".loop/notes.md",
        "STATUS: IN_PROGRESS\n\nSome important notes here.\n",
    );
    p.write(".loop/archive/stage-1-widget.md", "Next try path B.\n");

    ok!(&p.run(&["stash"]), "yoke stash");
    has!(
        &stderr(&p.run(&["stash", "log"])),
        "mode=brute",
        "stash log"
    );

    for gone in [
        ".loop/plan.md",
        ".loop/notes.md",
        ".loop/archive/stage-1-widget.md",
    ] {
        assert!(!p.exists(gone), "{gone} should be gone after stash");
    }
    let project_stash = stash_dir(&p);
    assert!(project_stash.join("index").exists(), "stash index");
    let backed_up = |rel: &str| {
        fs::read_dir(&project_stash)
            .unwrap()
            .flatten()
            .any(|e| e.path().join(rel).exists())
    };
    assert!(backed_up("plan.md"), "stashed plan.md");
    assert!(
        backed_up("archive/stage-1-widget.md"),
        "stashed archive note"
    );

    ok!(&p.run(&["stash", "pop"]), "yoke stash pop");
    has!(
        &p.read(".loop/plan.md"),
        "Build the widget",
        "restored plan"
    );
    has!(
        &p.read(".loop/archive/stage-1-widget.md"),
        "Next try path B",
        "restored archive"
    );
}

#[test]
fn stash_checkout_preserves_working_files_when_snapshot_is_corrupt() {
    let p = Project::new();
    ok!(&p.run(&["init"]), "yoke init");
    ok!(&p.run(&["stash"]), "yoke stash");
    let stash = stash_dir(&p);
    let index = fs::read_to_string(stash.join("index")).unwrap();
    let hash = index.split('|').next().unwrap();
    let snapshot = stash.join(hash);
    p.write(".loop/notes.md", "Keep these working notes.\n");

    fs::remove_file(snapshot.join("notes.md")).unwrap();
    let out = p.run(&["stash", "checkout", hash]);
    failed!(&out, "checkout of an incomplete snapshot");
    assert_eq!(p.read(".loop/notes.md"), "Keep these working notes.\n");
    assert!(
        !p.exists(".loop/plan.md"),
        "must not restore a partial snapshot"
    );

    fs::remove_dir_all(&snapshot).unwrap();
    fs::write(&snapshot, b"not a directory").unwrap();
    let out = p.run(&["stash", "checkout", hash]);
    failed!(&out, "checkout of a non-directory snapshot");
    assert_eq!(p.read(".loop/notes.md"), "Keep these working notes.\n");
    assert_eq!(fs::read_to_string(stash.join("index")).unwrap(), index);
}

#[test]
fn plan_loop_exits_on_status_done() {
    let p = Project::new();
    seed_loop(&p, "allow .\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(&["run", "--no-sandbox"], &[("PATH", &path)]);
    ok!(&out, "run on STATUS: DONE");
    has!(&stderr(&out), "loop complete", "run stderr");
}

#[test]
fn brute_judge_fail_then_pass() {
    let p = Project::new();
    seed_loop(&p, "allow .\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(
        &["run", "--no-sandbox"],
        &[("PATH", &path), ("MOCK_JUDGE", "fail-first")],
    );
    ok!(&out, "brute run after judge FAIL→PASS");
    assert_eq!(p.read(".loop/.judge-count").trim(), "2", "judge calls");
}

#[test]
fn guard_results_written_after_output_field_removal() {
    let p = Project::new();
    seed_loop(&p, "allow .\nguard  echo SENTINEL_GUARD_OUTPUT && exit 1\n");
    let out = p.run(&["run", "--no-sandbox", "--dry-run"]);
    failed!(&out, "dry-run with failing guard");
    let results = p.read(".loop/guard-results.md");
    has!(&results, "FAIL", "guard results");
    has!(&results, "SENTINEL_GUARD_OUTPUT", "guard results");
}

#[test]
fn brute_bailout_at_max_judge_failures() {
    let p = Project::new();
    seed_loop(&p, "allow .\nmax-judge-failures 2\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(
        &["run", "--no-sandbox"],
        &[("PATH", &path), ("MOCK_JUDGE", "fail-always")],
    );
    failed!(&out, "brute run with always-failing judge");
    has!(&stderr(&out), "bailing out", "run stderr");
    assert_eq!(
        p.read(".loop/.judge-count").trim(),
        "2",
        "judge fires max-judge-failures times"
    );
}

#[test]
fn judge_every_fires_on_done_and_exits() {
    let p = Project::new();
    seed_loop(&p, "allow .\njudge-every 5\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(&["run", "--no-sandbox"], &[("PATH", &path)]);
    ok!(&out, "judge-every run");
    has!(&stderr(&out), "Judge (DONE)", "run stderr");
    assert!(p.read(".loop/verdict.md").starts_with("VERDICT: PASS"));
}

#[test]
fn stash_records_correct_mode_after_extraction() {
    let p = Project::new();
    ok!(&p.run(&["init"]), "yoke init");
    p.write(".loop/plan.md", "## Stage 1\nDo it.\n");
    ok!(&p.run(&["stash"]), "yoke stash");

    let index = fs::read_to_string(stash_dir(&p).join("index")).expect("stash index");
    let first = index.lines().next().expect("one index line");
    let parts: Vec<&str> = first.splitn(4, '|').collect();
    assert!(parts.len() >= 3, "index line: {first:?}");
    assert_eq!(parts[2], "brute", "stash mode in index line {first:?}");
}
