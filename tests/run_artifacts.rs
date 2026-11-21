//! Behavioral test: a brute run captures a self-contained artifact directory
//! under `.loop/runs/<run-id>/`, including per-iteration diffs and the git
//! branch/SHA the patch was produced against. The harness must not commit.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn yoke_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"))
}

fn git(project: &std::path::Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail

for arg in "$@"; do
    if [[ "$arg" == *"judge.md"* ]]; then
        printf 'VERDICT: PASS\n\nartifact judge okay\n' > .loop/verdict.md
        exit 0
    fi
done

if [[ "$*" != *"--mode rpc"* ]]; then
    exit 0
fi

echo '{"type":"ready"}'
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        printf 'artifact smoke\n' > feature.txt
        printf 'STATUS: DONE\n\nFeature written.\n' > .loop/notes.md
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        cat <<'JSON'
{"type":"session","version":3,"id":"sess-artifacts","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"agent_end","messages":[]}
JSON
        exit 0
    elif [[ "$line" == *'"type":"steer"'* ]]; then
        printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
    fi
done
"#;

#[test]
fn run_captures_patch_and_vcs_metadata_without_committing() {
    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    fs::create_dir(&project).expect("create project");
    let project = project.as_path();

    git(project, &["init"]);
    fs::write(project.join("seed.txt"), "seed\n").unwrap();
    fs::write(project.join(".gitignore"), ".loop/\n").unwrap();
    git(project, &["add", ".gitignore", "seed.txt"]);
    git(project, &["commit", "-m", "seed"]);
    let branch = git(project, &["symbolic-ref", "--short", "HEAD"]);
    let base_sha = git(project, &["rev-parse", "HEAD"]);

    let init = Command::new(&yoke)
        .args(["init"])
        .current_dir(project)
        .output()
        .expect("yoke init");
    assert!(
        init.status.success(),
        "yoke init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(project.join(".loop/yoke.conf"), "metrics off\nallow .\n").unwrap();

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

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", test_path)
        .output()
        .expect("yoke run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke run should pass.\nstderr:\n{}",
        stderr
    );

    let runs_dir = project.join(".loop/runs");
    let run_dirs: Vec<_> = fs::read_dir(&runs_dir)
        .expect("read runs dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(run_dirs.len(), 1, "expected exactly one run directory");
    let run_dir = &run_dirs[0];

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(manifest["outcome"], "judge_pass");
    assert_eq!(manifest["iterations"], 1);
    assert_eq!(manifest["vcs"]["kind"], "git");
    assert_eq!(manifest["vcs"]["base_branch"], branch);
    assert_eq!(manifest["vcs"]["base_sha"], base_sha);
    assert!(
        run_dir.join("summary.md").is_file(),
        "summary.md should exist"
    );
    assert!(
        run_dir.join("snapshots/yoke.conf").is_file(),
        "config snapshot should exist"
    );
    assert!(
        run_dir.join("snapshots/plan.md").is_file(),
        "plan snapshot should exist"
    );

    let iter_dir = run_dir.join("iterations/0001");
    let iter_meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(iter_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(iter_meta["iteration"], 1);
    assert_eq!(iter_meta["guards_passed"], true);
    assert_eq!(iter_meta["status_done"], true);
    assert_eq!(iter_meta["vcs"]["base_sha"], base_sha);
    assert!(
        iter_dir.join("guard-results.md").is_file(),
        "guard results should be captured"
    );
    assert!(
        iter_dir.join("notes.md").is_file(),
        "notes snapshot should be captured"
    );

    let patch = fs::read_to_string(iter_dir.join("diff.patch")).unwrap();
    assert!(
        patch.contains("feature.txt") && patch.contains("+artifact smoke"),
        "iteration patch should contain the worker change.\npatch:\n{}",
        patch
    );
    assert!(
        !patch.contains(".loop/"),
        "patches must exclude yoke's own .loop/ state.\npatch:\n{}",
        patch
    );

    let final_patch = fs::read_to_string(run_dir.join("final.diff")).unwrap();
    assert!(
        final_patch.contains("feature.txt"),
        "final diff should contain the worker change.\npatch:\n{}",
        final_patch
    );

    // The harness captures patches; it must not create commits.
    assert_eq!(
        git(project, &["rev-list", "--count", "HEAD"]),
        "1",
        "yoke must not commit the worker's change"
    );
    let status = git(project, &["status", "--porcelain"]);
    assert!(
        status.contains("?? feature.txt"),
        "worker change should remain uncommitted, got:\n{}",
        status
    );

    // `yoke clean` resets working files but must preserve run artifacts.
    let clean = Command::new(&yoke)
        .args(["clean"])
        .current_dir(project)
        .output()
        .expect("yoke clean");
    assert!(
        clean.status.success(),
        "yoke clean failed: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(
        run_dir.join("run.json").is_file() && run_dir.join("final.diff").is_file(),
        "run artifacts should survive `yoke clean`"
    );
}
