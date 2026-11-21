//! Behavioral test: a completed yoke run persists per-iteration and per-run
//! metrics rows under the user's home directory (`~/.yoke/metrics/`) and the
//! rows survive `yoke clean`.
//!
//! Uses a mock `omp` script so no real agent invocation happens.  The
//! subprocess gets HOME pointed at the test tempdir so writes don't escape
//! the test sandbox.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn yoke_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"))
}

const PROTOCOL: &str = "\
# Protocol

Single-iteration test. Write STATUS: DONE and exit.
";

const PLAN: &str = "\
## Stage 1

Be done.
";

const CONF: &str = "\
allow .
";

const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail

MODE=""
for arg in "$@"; do
    if [[ "$arg" == "rpc" ]]; then MODE="rpc"; fi
done

if [[ "$MODE" == "rpc" ]]; then
    echo '{"type":"ready"}'
    while IFS= read -r line; do
        id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
        if [[ "$line" == *'"type":"prompt"'* ]]; then
            printf 'STATUS: DONE\n\n## Stage 1\nDone.\n' > .loop/notes.md
            printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
            cat <<'JSON'
{"type":"session","version":3,"id":"sess-metrics","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"model":"gpt-test","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12,"cost":{"input":0.1,"output":0.15,"cacheRead":0,"cacheWrite":0,"total":0.25}},"duration":1500}}
{"type":"agent_end","messages":[]}
JSON
        elif [[ "$line" == *'"type":"steer"'* ]]; then
            printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
        fi
    done
    exit 0
fi

printf 'STATUS: DONE\n\n## Stage 1\nDone.\n' > .loop/notes.md
cat <<'JSON'
{"type":"session","version":3,"id":"sess-metrics","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"model":"gpt-test","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12,"cost":{"input":0.1,"output":0.15,"cacheRead":0,"cacheWrite":0,"total":0.25}},"duration":1500}}
{"type":"agent_end","messages":[]}
JSON
exit 0
"#;

#[test]
fn metrics_rows_persist_under_home_and_survive_clean() {
    // Build yoke
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("proj");
    fs::create_dir(&project).expect("create proj");

    // Isolated HOME so metrics land under tmp/.yoke/metrics/...
    let fake_home = tmp.path().join("home");
    fs::create_dir(&fake_home).expect("create home");

    // Project files
    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).expect("create .loop");
    fs::write(loop_dir.join("protocol.md"), PROTOCOL).unwrap();
    fs::write(loop_dir.join("plan.md"), PLAN).unwrap();
    fs::write(loop_dir.join("yoke.conf"), CONF).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    // Mock OMP on PATH
    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).expect("create mock-bin");
    let mock_omp_path = mock_bin_dir.join("omp");
    fs::write(&mock_omp_path, MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Boundary check needs a git repo
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&project)
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
    fs::write(project.join("seed.txt"), "x\n").unwrap();
    git(&["add", "seed.txt"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "init",
    ]);

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(&project)
        .env("PATH", &test_path)
        .env("HOME", &fake_home)
        .output()
        .expect("failed to run yoke");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke should exit 0 on STATUS: DONE.\nstderr:\n{}",
        stderr
    );

    // Find the metrics directory: ~/.yoke/metrics/<project-slug>/
    let metrics_dir = fake_home.join(".yoke").join("metrics");
    assert!(
        metrics_dir.exists(),
        "metrics dir should be created at {}",
        metrics_dir.display()
    );
    let project_dirs: Vec<_> = fs::read_dir(&metrics_dir)
        .expect("read metrics dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(
        project_dirs.len(),
        1,
        "expected exactly one project slug dir, got {:?}",
        project_dirs
    );
    let slug_dir = &project_dirs[0];

    // The slug dir must contain runs.ndjson + at least one <run-id>.ndjson
    let mut runs_path = None;
    let mut iter_path = None;
    for entry in fs::read_dir(slug_dir).expect("read slug dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "runs.ndjson" {
            runs_path = Some(entry.path());
        } else if name.ends_with(".ndjson") {
            iter_path = Some(entry.path());
        }
    }
    let runs_path = runs_path.expect("runs.ndjson should exist");
    let iter_path = iter_path.expect("iteration ndjson file should exist");

    // Run row: contains outcome=done and iterations >= 1
    let runs_content = fs::read_to_string(&runs_path).expect("read runs.ndjson");
    assert!(
        runs_content.contains("\"outcome\":\"done\""),
        "runs.ndjson should record outcome=done.\ncontent: {}",
        runs_content
    );
    assert!(
        runs_content.contains("\"iterations\":1"),
        "runs.ndjson should report 1 iteration.\ncontent: {}",
        runs_content
    );

    // Iteration row: has mode=loop, status_done=true, restore_ms field
    let iter_content = fs::read_to_string(&iter_path).expect("read iter ndjson");
    assert!(
        iter_content.contains("\"mode\":\"loop\""),
        "iter row should carry mode=loop.\ncontent: {}",
        iter_content
    );
    assert!(
        iter_content.contains("\"status_done\":true"),
        "iter row should record status_done=true.\ncontent: {}",
        iter_content
    );
    assert!(
        iter_content.contains("\"restore_ms\":"),
        "iter row should record restore_ms.\ncontent: {}",
        iter_content
    );
    assert!(
        iter_content.contains("\"cost_usd\":0.25"),
        "iter row should record OMP stream cost.\ncontent: {}",
        iter_content
    );
    assert!(
        iter_content.contains("\"num_turns\":1"),
        "iter row should record OMP turn count.\ncontent: {}",
        iter_content
    );

    // Survive `yoke clean`
    let clean_output = Command::new(&yoke)
        .args(["clean"])
        .current_dir(&project)
        .env("HOME", &fake_home)
        .output()
        .expect("yoke clean");
    assert!(clean_output.status.success(), "yoke clean failed");
    assert!(
        runs_path.exists(),
        "runs.ndjson must survive yoke clean (lives outside the project)"
    );
    assert!(
        iter_path.exists(),
        "iteration ndjson must survive yoke clean"
    );

    // `yoke stats` lists the run we just recorded
    let stats_output = Command::new(&yoke)
        .args(["stats"])
        .current_dir(&project)
        .env("HOME", &fake_home)
        .output()
        .expect("yoke stats");
    assert!(stats_output.status.success(), "yoke stats failed");
    let stats_stdout = String::from_utf8_lossy(&stats_output.stdout);
    let stats_stderr = String::from_utf8_lossy(&stats_output.stderr);
    let combined = format!("{}{}", stats_stdout, stats_stderr);
    // Derive run-id from the file name we found earlier
    let run_id = iter_path
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("run id from file stem");
    assert!(
        combined.contains(run_id),
        "yoke stats should list the run-id {} in its output.\ncombined:\n{}",
        run_id,
        combined
    );
    assert!(
        combined.contains("done"),
        "yoke stats should show the run outcome 'done'.\ncombined:\n{}",
        combined
    );

    // `yoke stats --run <id>` shows the iteration table
    let run_output = Command::new(&yoke)
        .args(["stats", "--run", run_id])
        .current_dir(&project)
        .env("HOME", &fake_home)
        .output()
        .expect("yoke stats --run");
    assert!(run_output.status.success(), "yoke stats --run failed");
    let run_stdout = String::from_utf8_lossy(&run_output.stdout);
    let run_stderr = String::from_utf8_lossy(&run_output.stderr);
    let run_combined = format!("{}{}", run_stdout, run_stderr);
    assert!(
        run_combined.contains("ITER"),
        "yoke stats --run should print an iteration table header.\noutput:\n{}",
        run_combined
    );
    assert!(
        // We produced one iteration; the row should show iteration "1"
        // and one of the "pass" / "yes" status badges.
        run_combined.contains("pass"),
        "yoke stats --run should report guard status.\noutput:\n{}",
        run_combined
    );
}
