//! Integration tests: adversarial scenarios targeting the cleaner's refactoring.
//!
//! These tests focus on:
//! 1. Stash extraction: does `yoke stash` / `yoke clean` still correctly
//!    snapshot and restore .loop/ files after stash code moved to stash.rs?
//! 2. is_status_done refactor: does the plan loop correctly detect STATUS: DONE
//!    when the generalized function is used instead of the old hardcoded one?
//! 3. Saga mode: does the refactored is_status_done(SAGA_NOTES_PATH) correctly
//!    detect scoper completion (vs the old hardcoded is_saga_done)?
//! 4. Guard results file: after removing GuardResult.output field, does the
//!    guard results markdown file still get written with pass/fail content?

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// Build the yoke binary path (relies on `cargo test` putting it in target/).
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

fn project_stash_dir(home: &std::path::Path) -> std::path::PathBuf {
    let stash_root = home.join(".yoke/stash");
    let mut dirs: Vec<std::path::PathBuf> = fs::read_dir(&stash_root)
        .unwrap_or_else(|e| panic!("stash root {} should exist: {}", stash_root.display(), e))
        .map(|entry| entry.expect("stash root entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(
        dirs.len(),
        1,
        "stash root should contain one project-slug directory, got: {:?}",
        dirs
    );
    dirs.remove(0)
}

/// Set up a minimal git repo in the given directory.
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
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init"]);
    fs::write(project.join("dummy.txt"), "seed\n").unwrap();
    git(&["add", "dummy.txt"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@test",
        "commit",
        "-m",
        "init",
    ]);
}

// ── Test 1: stash + clean round-trip after extraction ──────────────────

/// After extracting stash code to stash.rs, verify that `yoke clean`
/// still auto-stashes working files and that `yoke stash pop` restores them.
/// A broken extraction could lose file data or corrupt the stash index.
#[test]
fn stash_roundtrip_after_extraction() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    let home = project.join("home");
    git_init(project);

    // Initialize brute mode (creates .loop/ with judge.md, etc.)
    let out = Command::new(&yoke)
        .args(["init", "brute"])
        .current_dir(project)
        .output()
        .expect("yoke init brute");
    assert!(
        out.status.success(),
        "yoke init brute failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Write distinctive content into plan.md, notes.md, and the detailed archive
    let loop_dir = project.join(".loop");
    fs::write(
        loop_dir.join("plan.md"),
        "## Stage 1 — Build the widget\n\nDo the thing.\n",
    )
    .unwrap();
    fs::write(
        loop_dir.join("notes.md"),
        "STATUS: IN_PROGRESS\n\nSome important notes here.\n",
    )
    .unwrap();
    fs::write(
        loop_dir.join("archive/stage-1-widget.md"),
        "Tried path A; next try path B.\n",
    )
    .unwrap();

    // Stash the current state
    let out = Command::new(&yoke)
        .args(["stash"])
        .current_dir(project)
        .env("HOME", &home)
        .output()
        .expect("yoke stash");
    assert!(
        out.status.success(),
        "yoke stash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify stash log shows an entry
    let out = Command::new(&yoke)
        .args(["stash", "log"])
        .current_dir(project)
        .env("HOME", &home)
        .output()
        .expect("yoke stash log");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mode=brute"),
        "stash log should show mode=brute, got:\n{}",
        stderr
    );

    // Verify stash cleared .loop/ working files
    assert!(
        !loop_dir.join("plan.md").exists(),
        "plan.md should be gone after stash"
    );
    assert!(
        !loop_dir.join("notes.md").exists(),
        "notes.md should be gone after stash"
    );
    assert!(
        !loop_dir.join("archive/stage-1-widget.md").exists(),
        "archive stage note should be gone after stash"
    );
    let project_stash = project_stash_dir(&home);
    assert!(
        project_stash.join("index").exists(),
        "project-scoped stash index should exist"
    );
    let has_plan_backup = fs::read_dir(&project_stash)
        .expect("project stash dir")
        .any(|entry| entry.expect("stash entry").path().join("plan.md").exists());
    assert!(
        has_plan_backup,
        "stashed files should live under ~/.yoke/stash/<project-slug>/<hash>/"
    );
    let has_archive_backup = fs::read_dir(&project_stash)
        .expect("project stash dir")
        .any(|entry| {
            entry
                .expect("stash entry")
                .path()
                .join("archive/stage-1-widget.md")
                .exists()
        });
    assert!(
        has_archive_backup,
        "stashed archive files should live under ~/.yoke/stash/<project-slug>/<hash>/archive/"
    );
    assert!(
        !loop_dir.join(".stash").exists(),
        ".loop/.stash should not be used for stash storage"
    );

    // Pop — should restore the stashed state with our distinctive content
    let out = Command::new(&yoke)
        .args(["stash", "pop"])
        .current_dir(project)
        .env("HOME", &home)
        .output()
        .expect("yoke stash pop");
    assert!(
        out.status.success(),
        "yoke stash pop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify round-trip: files restored with original content
    let plan = fs::read_to_string(loop_dir.join("plan.md")).unwrap();
    assert!(
        plan.contains("Build the widget"),
        "plan.md should be restored after pop, got: {:?}",
        plan
    );
    let archive = fs::read_to_string(loop_dir.join("archive/stage-1-widget.md")).unwrap();
    assert!(
        archive.contains("next try path B"),
        "archive note should be restored after pop, got: {:?}",
        archive
    );
}

// ── Test 2: plan loop exits on STATUS: DONE with generalized is_status_done ──

/// The cleaner changed `is_done()` (hardcoded to NOTES_PATH) into
/// `is_status_done(path)`. If any call site mistakenly passes the wrong path,
/// the loop would spin forever or exit prematurely.
///
/// This test verifies: agent writes STATUS: DONE → yoke exits 0.
#[test]
fn plan_loop_exits_on_status_done() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    // Minimal loop-mode setup (no judge.md → loop mode, not brute)
    let protocol = "\
# Protocol
Read plan.md, implement it, then set STATUS: DONE in notes.md.
";
    let conf = "allow .\n";
    let plan = "## Stage 1 — Do something\nJust touch a file.\n";

    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("plan.md"), plan).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    // Mock OMP: immediately writes STATUS: DONE and exits
    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();

    let mock_omp = r#"#!/usr/bin/env bash
set -euo pipefail
# Always signal done immediately
printf 'STATUS: DONE\n' > .loop/notes.md
exit 0
"#;
    let mock_path = mock_bin_dir.join("omp");
    fs::write(&mock_path, mock_omp).unwrap();
    fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // yoke should exit 0 — the generalized is_status_done(NOTES_PATH) found DONE
    assert!(
        output.status.success(),
        "yoke should exit 0 when agent signals STATUS: DONE.\n\
         Exit code: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );

    // Verify the "loop complete" message appears
    assert!(
        stderr.contains("loop complete"),
        "stderr should contain 'loop complete', got:\n{}",
        stderr,
    );
}

// ── Test 3: brute mode still invokes judge and handles FAIL→PASS correctly ──

/// After the stash extraction and is_done→is_status_done refactor, verify
/// the brute loop still: runs agent → runs judge → on FAIL retries → on PASS exits.
/// This is the same scenario as brute_verdict but run against the refactored code.
#[test]
fn brute_judge_fail_then_pass() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    let protocol = "\
# Protocol
Read plan, implement, set STATUS: DONE in notes.md.
";
    let plan = "## Stage 1 — Implement\nDo the feature.\n";
    let judge = "# Judge\nVerify the feature.\n\n## Verdict\nWrite verdict.\n";
    let conf = "allow .\n";

    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("plan.md"), plan).unwrap();
    fs::write(loop_dir.join("judge.md"), judge).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("verdict.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();

    // Mock OMP: agent writes STATUS: DONE, judge FAILs once then PASSes
    let mock_omp = r#"#!/usr/bin/env bash
set -euo pipefail
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "protocol.md"; then
    printf 'STATUS: DONE\n' > .loop/notes.md
elif echo "$PROMPT" | grep -q "judge.md"; then
    COUNTER=".loop/.judge-count"
    N=0
    if [[ -f "$COUNTER" ]]; then N=$(cat "$COUNTER"); fi
    N=$((N + 1))
    echo "$N" > "$COUNTER"
    if [[ "$N" -eq 1 ]]; then
        printf 'VERDICT: FAIL\n\nNot good enough.' > .loop/verdict.md
    else
        printf 'VERDICT: PASS\n\nLooks great.' > .loop/verdict.md
    fi
fi
exit 0
"#;
    let mock_path = mock_bin_dir.join("omp");
    fs::write(&mock_path, mock_omp).unwrap();
    fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should exit 0 — judge eventually said PASS
    assert!(
        output.status.success(),
        "yoke should exit 0 after judge PASS.\nExit: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );

    // Judge should have been called exactly 2 times
    let judge_count = fs::read_to_string(loop_dir.join(".judge-count")).unwrap();
    assert_eq!(
        judge_count.trim(),
        "2",
        "judge should be called exactly twice (FAIL then PASS), got: {:?}",
        judge_count.trim(),
    );
}

// ── Test 4: saga exits on STATUS: DONE in saga-notes.md (not notes.md) ──

/// The cleaner replaced the hardcoded `is_saga_done()` (which read SAGA_NOTES_PATH)
/// with the generic `is_status_done(SAGA_NOTES_PATH)`. If a refactoring mistake
/// accidentally passes NOTES_PATH instead, the saga loop would either spin forever
/// (scoper keeps writing DONE to saga-notes.md but yoke checks notes.md) or
/// would exit prematurely based on the inner brute worker's notes.md.
///
/// This test sets up saga mode, mocks the scoper to write STATUS: DONE to
/// saga-notes.md on the first cycle, and verifies yoke exits 0 with the
/// "saga complete" message.
#[test]
fn saga_exits_on_saga_notes_done() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    // Saga mode files
    let saga_protocol = "# Saga Protocol\nRead specification.md, decompose into sub-plans.\n";
    let protocol = "# Worker Protocol\nRead plan, implement, set STATUS: DONE.\n";
    let judge = "# Judge\nVerify the sub-plan.\n\n## Verdict\nWrite verdict.\n";
    let conf = "allow .\n";
    let specification = "# Spec\nBuild a widget that does X.\n";

    fs::write(loop_dir.join("saga-protocol.md"), saga_protocol).unwrap();
    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("judge.md"), judge).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("specification.md"), specification).unwrap();
    fs::write(loop_dir.join("saga-notes.md"), "").unwrap();
    fs::write(loop_dir.join("decisions.md"), "").unwrap();
    fs::write(loop_dir.join("sub-plan.md"), "").unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("verdict.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    // Mock OMP: scoper writes STATUS: DONE to saga-notes.md immediately.
    // Critically: notes.md is left empty — if yoke checks notes.md instead of
    // saga-notes.md, it would NOT see DONE and would spin forever.
    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();

    let mock_omp = r#"#!/usr/bin/env bash
set -euo pipefail
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "saga-protocol.md"; then
    # Scoper: signal DONE via saga-notes.md
    printf 'STATUS: DONE\n\nAll chunks complete.\n' > .loop/saga-notes.md
fi
exit 0
"#;
    let mock_path = mock_bin_dir.join("omp");
    fs::write(&mock_path, mock_omp).unwrap();
    fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // yoke should exit 0 — is_status_done(SAGA_NOTES_PATH) found DONE
    assert!(
        output.status.success(),
        "yoke should exit 0 when scoper signals DONE in saga-notes.md.\n\
         Exit code: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );

    // Verify the "saga complete" message appears
    assert!(
        stderr.contains("saga complete"),
        "stderr should contain 'saga complete', got:\n{}",
        stderr,
    );

    // notes.md should still be empty — confirms yoke checked saga-notes.md, not notes.md
    let notes = fs::read_to_string(loop_dir.join("notes.md")).unwrap();
    assert!(
        notes.is_empty(),
        "notes.md should be empty (saga checks saga-notes.md), got: {:?}",
        notes,
    );
}

// ── Test 5: guard results file written correctly after output field removal ──

/// The cleaner removed the `output` field from `GuardResult`. If the guard
/// results markdown file writing was accidentally broken by this change,
/// the agent would lose visibility into guard pass/fail details on the next
/// iteration — a silent data loss that could cause infinite loops.
///
/// This test uses dry-run mode with a guard that fails, and verifies the
/// guard-results.md file contains the expected FAIL section with output.
#[test]
fn guard_results_written_after_output_field_removal() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    // Loop mode with a guard that deliberately fails with distinctive output
    let protocol = "# Protocol\nDo the thing.\n";
    let conf = "\
allow .
guard  echo SENTINEL_GUARD_OUTPUT && exit 1
";
    let plan = "## Stage 1\nDo it.\n";

    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("plan.md"), plan).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    // Dry-run: no OMP invocation, but guards still execute
    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox", "--dry-run"])
        .current_dir(project)
        .output()
        .expect("yoke run --dry-run");

    // Guard failed so yoke exits non-zero in dry-run
    assert!(
        !output.status.success(),
        "yoke should exit non-zero when guard fails in dry-run"
    );

    // Verify guard-results.md was written with the guard output
    let results = fs::read_to_string(loop_dir.join("guard-results.md")).unwrap();

    assert!(
        results.contains("FAIL"),
        "guard-results.md should contain 'FAIL' for the failing guard.\nGot:\n{}",
        results,
    );
    assert!(
        results.contains("SENTINEL_GUARD_OUTPUT"),
        "guard-results.md should contain the guard's stdout ('SENTINEL_GUARD_OUTPUT').\n\
         If this is missing, the output field removal broke results file writing.\nGot:\n{}",
        results,
    );
}

// ── Test 6: brute bailout fires at exactly max-judge-failures ───────────

/// The cleaner extracted the bailout threshold check into `is_judge_bailout()`.
/// If the comparison operator was changed (e.g. `>` instead of `>=`), the brute
/// loop would either bail one iteration too late (wasting an API call) or too
/// early (never giving the worker a fair chance).
///
/// This test configures `max-judge-failures 2` and has the judge always FAIL.
/// Expects: yoke exits non-zero after exactly 2 judge failures (2 brute iterations).
#[test]
fn brute_bailout_at_max_judge_failures() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    let protocol = "\
# Protocol
Read plan, implement, set STATUS: DONE in notes.md.
";
    let plan = "## Stage 1 — Implement\nDo the feature.\n";
    let judge = "# Judge\nVerify the feature.\n\n## Verdict\nWrite verdict.\n";
    // max-judge-failures 2 — should bail after exactly 2 consecutive judge FAILs
    let conf = "\
allow .
max-judge-failures 2
";

    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("plan.md"), plan).unwrap();
    fs::write(loop_dir.join("judge.md"), judge).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("verdict.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();

    // Mock OMP: agent always writes STATUS: DONE, judge always FAILs.
    // Tracks call counts so we can assert the exact number of iterations.
    let mock_omp = r#"#!/usr/bin/env bash
set -euo pipefail
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "protocol.md"; then
    COUNTER=".loop/.agent-count"
    N=0
    if [[ -f "$COUNTER" ]]; then N=$(cat "$COUNTER"); fi
    N=$((N + 1))
    echo "$N" > "$COUNTER"
    printf 'STATUS: DONE\n' > .loop/notes.md
elif echo "$PROMPT" | grep -q "judge.md"; then
    COUNTER=".loop/.judge-count"
    N=0
    if [[ -f "$COUNTER" ]]; then N=$(cat "$COUNTER"); fi
    N=$((N + 1))
    echo "$N" > "$COUNTER"
    # Always FAIL
    printf 'VERDICT: FAIL\n\nStill broken.\n' > .loop/verdict.md
fi
exit 0
"#;
    let mock_path = mock_bin_dir.join("omp");
    fs::write(&mock_path, mock_omp).unwrap();
    fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // yoke should exit non-zero (bailout)
    assert!(
        !output.status.success(),
        "yoke should exit non-zero after max judge failures.\nExit: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );

    // Verify stderr mentions bailing out
    assert!(
        stderr.contains("bailing out"),
        "stderr should mention 'bailing out', got:\n{}",
        stderr,
    );

    // Judge should have been called exactly 2 times (matching max-judge-failures)
    let judge_count = fs::read_to_string(loop_dir.join(".judge-count")).unwrap();
    assert_eq!(
        judge_count.trim(),
        "2",
        "judge should be called exactly 2 times (max-judge-failures=2), got: {:?}\nStderr:\n{}",
        judge_count.trim(),
        stderr,
    );
}

// ── Test 7: judge-every fires judge on DONE and exits on PASS ──────────

/// The cleaner extracted the judge-every logic into `evaluate_judge_every()`.
/// If the extraction broke the DONE→judge→PASS→exit path, the plan loop
/// would either: never invoke the judge (spinning forever), invoke it but
/// ignore the PASS (spinning forever), or skip the judge and exit without
/// verification (silent quality regression).
///
/// This test configures `judge-every 5` in loop mode with a judge.md present.
/// The agent signals DONE on iteration 1 (before cadence 5), so the judge
/// should fire because DONE always triggers the judge regardless of cadence.
/// The judge returns PASS, so yoke should exit 0.
#[test]
fn judge_every_fires_on_done_and_exits() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();

    let protocol = "\
# Protocol
Read plan, implement, set STATUS: DONE in notes.md.
";
    let plan = "## Stage 1 — Implement\nDo the feature.\n";
    let judge = "# Judge\nVerify the feature.\n\n## Verdict\nWrite verdict.\n";
    // judge-every 5: cadence is 5, but DONE should fire judge immediately
    let conf = "\
allow .
judge-every 5
";

    fs::write(loop_dir.join("protocol.md"), protocol).unwrap();
    fs::write(loop_dir.join("yoke.conf"), conf).unwrap();
    fs::write(loop_dir.join("plan.md"), plan).unwrap();
    fs::write(loop_dir.join("judge.md"), judge).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("verdict.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();

    // Mock OMP: agent immediately signals DONE, judge immediately returns PASS
    let mock_omp = r#"#!/usr/bin/env bash
set -euo pipefail
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "protocol.md"; then
    printf 'STATUS: DONE\n' > .loop/notes.md
elif echo "$PROMPT" | grep -q "judge.md"; then
    printf 'VERDICT: PASS\n\nAll good.\n' > .loop/verdict.md
fi
exit 0
"#;
    let mock_path = mock_bin_dir.join("omp");
    fs::write(&mock_path, mock_omp).unwrap();
    fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // yoke should exit 0 — judge-every detected DONE and judge said PASS
    assert!(
        output.status.success(),
        "yoke should exit 0 when judge-every fires on DONE and judge PASSes.\n\
         Exit code: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );

    // Verify the judge was actually invoked (not skipped)
    assert!(
        stderr.contains("Judge (DONE)"),
        "stderr should show 'Judge (DONE)' banner (judge fired on worker DONE), got:\n{}",
        stderr,
    );

    // Verify verdict.md has PASS
    let verdict = fs::read_to_string(loop_dir.join("verdict.md")).unwrap();
    assert!(
        verdict.starts_with("VERDICT: PASS"),
        "verdict.md should contain PASS, got: {:?}",
        verdict,
    );
}

// ── Test 8: stash mode tag preserved after module extraction ────────────

/// After stash code was extracted to stash.rs, the `mode` parameter is now
/// passed from main.rs rather than calling `detect_mode()` internally.
/// If the caller passes the wrong mode, stash entries would have incorrect
/// mode tags, making `yoke stash log` misleading and potentially breaking
/// mode-aware restoration logic.
///
/// This test initializes brute mode, creates a stash, and verifies the
/// stash index records "brute" (not "unknown" or empty).
#[test]
fn stash_records_correct_mode_after_extraction() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    let home = project.join("home");
    git_init(project);

    // Initialize brute mode
    let out = Command::new(&yoke)
        .args(["init", "brute"])
        .current_dir(project)
        .output()
        .expect("yoke init brute");
    assert!(
        out.status.success(),
        "yoke init brute failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Write some content so stash has something to snapshot
    fs::write(project.join(".loop/plan.md"), "## Stage 1\nDo it.\n").unwrap();

    // Create a stash
    let out = Command::new(&yoke)
        .args(["stash"])
        .current_dir(project)
        .env("HOME", &home)
        .output()
        .expect("yoke stash");
    assert!(
        out.status.success(),
        "yoke stash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Read the project-scoped stash index directly and verify mode=brute
    let index_path = project_stash_dir(&home).join("index");
    assert!(
        index_path.exists(),
        "stash index should exist after stashing"
    );

    let index = fs::read_to_string(&index_path).unwrap();
    // Index format: hash|timestamp|mode|file1,file2,...
    let first_line = index
        .lines()
        .next()
        .expect("index should have at least one line");
    let parts: Vec<&str> = first_line.splitn(4, '|').collect();
    assert!(
        parts.len() >= 3,
        "index line should have at least 3 pipe-separated fields, got: {:?}",
        first_line,
    );
    assert_eq!(
        parts[2], "brute",
        "stash mode should be 'brute' (not 'unknown' or empty). \
         If this fails, the mode parameter is not being passed correctly \
         from main.rs to stash.rs after extraction.\nIndex line: {:?}",
        first_line,
    );
}
