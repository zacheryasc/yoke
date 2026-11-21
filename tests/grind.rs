use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn yoke_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yoke"))
}

fn build_yoke() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");
}

fn git(project: &Path, args: &[&str]) {
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
}

fn seed_project(project: &Path) {
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"grind-subject\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    fs::create_dir(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), "pub fn hot() -> u32 { 1 }\n").unwrap();

    git(project, &["init"]);
    git(project, &["add", "Cargo.toml", "src/lib.rs"]);
    git(
        project,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "-m",
            "init",
        ],
    );
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn mock_path(mock_bin_dir: &Path) -> String {
    let original_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", mock_bin_dir.display(), original_path)
}

fn constant_cstat(cost: f64) -> String {
    let score = format!("{cost:.1}");
    format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nprintf '{{\"cstat_version\":\"fake\",\"score_version\":\"code_complexity_cost_v0\",\"target\":\".\",\"code_complexity_cost\":{score},\"top_contributors\":[{{\"kind\":\"function\",\"file\":\"src/lib.rs\",\"function\":\"hot\",\"cost\":{score}}}]}}\\n'\n"
    )
}

fn run_yoke_init_grind(yoke: &Path, project: &Path, test_path: &str) -> std::process::Output {
    Command::new(yoke)
        .args(["init", "grind"])
        .current_dir(project)
        .env("PATH", test_path)
        .output()
        .expect("yoke init grind")
}

#[test]
fn init_grind_creates_profile_and_captures_baseline() {
    build_yoke();
    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_project(project);

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    write_executable(&mock_bin_dir.join("cstat"), &constant_cstat(5.0));
    let test_path = mock_path(&mock_bin_dir);

    let output = run_yoke_init_grind(&yoke, project, &test_path);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke init grind should exit 0. exit={:?}\nstderr:\n{}",
        output.status.code(),
        stderr
    );

    let loop_dir = project.join(".loop");
    for name in [
        "grind-gate.py",
        "cstat-baseline.json",
        "protocol.md",
        "yoke.conf",
        "notes.md",
        "guard-results.md",
        "judge.md",
        "verdict.md",
    ] {
        assert!(loop_dir.join(name).exists(), "missing .loop/{name}");
    }
    assert!(
        loop_dir.join("archive").is_dir(),
        "missing .loop/archive/ note archive"
    );
    for name in ["grind.md", "plan.md"] {
        assert!(!loop_dir.join(name).exists(), "unexpected .loop/{name}");
    }

    let baseline = fs::read_to_string(loop_dir.join("cstat-baseline.json")).unwrap();
    let compact: String = baseline.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("\"code_complexity_cost\":5.0"),
        "baseline should contain captured scorecard cost. baseline:\n{}",
        baseline
    );

    let conf = fs::read_to_string(loop_dir.join("yoke.conf")).unwrap();
    assert!(
        conf.contains("guard       python3 .loop/grind-gate.py"),
        "grind guard must be configured. yoke.conf:\n{}",
        conf
    );
    assert!(
        conf.contains("grind-passes 1"),
        "grind pass count must default to one. yoke.conf:\n{}",
        conf
    );
    assert!(
        conf.contains("# judge-every 3"),
        "grind config should document optional judge cadence. yoke.conf:\n{}",
        conf
    );
    assert!(
        conf.contains("# no-modify   tests/"),
        "tests no-modify example should be commented. yoke.conf:\n{}",
        conf
    );
    assert!(
        !conf
            .lines()
            .any(|line| line.trim_start().starts_with("no-modify   tests/")),
        "tests no-modify rule must not be active by default. yoke.conf:\n{}",
        conf
    );

    let protocol = fs::read_to_string(loop_dir.join("protocol.md")).unwrap();
    assert!(
        !protocol.contains(".loop/plan.md"),
        "grind protocol must not reference plan.md. protocol:\n{}",
        protocol
    );
    assert!(
        protocol.contains("grind-passes"),
        "grind protocol should document pass-count completion. protocol:\n{}",
        protocol
    );
    assert!(
        protocol.contains("grind mode ignores it for completion"),
        "grind protocol should remove status-based completion. protocol:\n{}",
        protocol
    );
    assert!(
        protocol.contains(".loop/archive/<name>.md"),
        "grind protocol should document the secondary note archive. protocol:\n{}",
        protocol
    );
    for command in [
        "loc",
        "symbols",
        "branching",
        "signature",
        "span",
        "scorecard",
        "deps",
        "dead-code",
        "test-reachability",
        "call-trace",
        "coverage",
        "cluster",
    ] {
        assert!(
            protocol.contains(&format!("`{command}`")),
            "grind protocol should document cstat {command}. protocol:\n{}",
            protocol
        );
    }
    assert!(
        protocol.contains("Do not stop after one simplification"),
        "grind protocol should push agents past timid one-edit cleanup. protocol:\n{}",
        protocol
    );
}

#[test]
fn grind_dry_run_rejects_without_cstat_improvement() {
    build_yoke();
    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_project(project);

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    write_executable(&mock_bin_dir.join("cstat"), &constant_cstat(5.0));
    let test_path = mock_path(&mock_bin_dir);

    let init = run_yoke_init_grind(&yoke, project, &test_path);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let output = Command::new(&yoke)
        .args(["run", "--dry-run"])
        .current_dir(project)
        .env("PATH", &test_path)
        .env("GRIND_ORACLE", "true")
        .output()
        .expect("yoke run --dry-run");
    assert!(
        !output.status.success(),
        "dry run should reject unchanged cstat score"
    );

    let guard_results = fs::read_to_string(project.join(".loop/guard-results.md")).unwrap();
    assert!(
        guard_results.contains("GRIND: FAIL"),
        "guard results should show grind failure. guard-results:\n{}",
        guard_results
    );
    assert!(
        guard_results.contains("cstat scorecard did not improve"),
        "guard results should explain unchanged score. guard-results:\n{}",
        guard_results
    );
}

#[test]
fn grind_counts_gate_passes_without_status_and_runs_optional_judge() {
    build_yoke();
    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_project(project);

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    write_executable(
        &mock_bin_dir.join("cstat"),
        r#"#!/usr/bin/env bash
set -euo pipefail
count_file=".cstat-count"
count=0
if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
if [ "$count" -eq 1 ]; then
    cost=10.0
else
    cost=8.0
fi
printf '{"cstat_version":"fake","score_version":"code_complexity_cost_v0","target":".","code_complexity_cost":%s,"top_contributors":[{"kind":"function","file":"src/lib.rs","function":"hot","cost":%s}]}\n' "$cost" "$cost"
"#,
    );
    write_executable(
        &mock_bin_dir.join("omp"),
        r#"#!/usr/bin/env bash
set -euo pipefail
args="$*"
if [[ "$args" == *".loop/judge.md"* ]]; then
    count_file=".judge-count"
    count=0
    if [ -f "$count_file" ]; then
        count=$(cat "$count_file")
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    cat > .loop/verdict.md <<'EOF'
VERDICT: PASS

grind gate passed and cleanup remains acceptable
EOF
    exit 0
fi

if [[ "$args" == *"--mode rpc"* ]]; then
    echo '{"type":"ready"}'
    while IFS= read -r line; do
        id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
        if [[ "$line" == *'"type":"prompt"'* ]]; then
            count_file=".worker-count"
            count=0
            if [ -f "$count_file" ]; then
                count=$(cat "$count_file")
            fi
            count=$((count + 1))
            printf '%s\n' "$count" > "$count_file"
            if [ "$count" -gt 2 ]; then
                exit 42
            fi
            printf 'pass %s completed without status\n' "$count" > .loop/notes.md
            printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
            cat <<'JSON'
{"type":"session","version":3,"id":"sess-grind","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"agent_end","messages":[]}
JSON
        elif [[ "$line" == *'"type":"steer"'* ]]; then
            printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
        fi
    done
    exit 0
fi

count_file=".worker-count"
count=0
if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
if [ "$count" -gt 2 ]; then
    exit 42
fi
printf 'pass %s completed without status\n' "$count" > .loop/notes.md
exit 0
"#,
    );
    let test_path = mock_path(&mock_bin_dir);

    let init = run_yoke_init_grind(&yoke, project, &test_path);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let conf_path = project.join(".loop/yoke.conf");
    let conf = fs::read_to_string(&conf_path)
        .unwrap()
        .replace("grind-passes 1", "grind-passes 2\njudge-every 1");
    fs::write(&conf_path, conf).unwrap();

    let output = Command::new(&yoke)
        .args(["run"])
        .current_dir(project)
        .env("PATH", &test_path)
        .env("GRIND_ORACLE", "true")
        .output()
        .expect("yoke run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke run should complete two judged grind passes. exit={:?}\nstderr:\n{}",
        output.status.code(),
        stderr
    );

    let worker_count = fs::read_to_string(project.join(".worker-count")).unwrap();
    assert_eq!(
        worker_count.trim(),
        "2",
        "grind-passes=2 should invoke exactly two workers"
    );
    let judge_count = fs::read_to_string(project.join(".judge-count")).unwrap();
    assert_eq!(
        judge_count.trim(),
        "2",
        "judge-every=1 should judge each successful grind pass"
    );
    let notes = fs::read_to_string(project.join(".loop/notes.md")).unwrap();
    assert!(
        !notes.starts_with("STATUS:"),
        "grind completion must not depend on STATUS notes. notes:\n{}",
        notes
    );
}

#[test]
fn grind_restores_gate_and_baseline_before_guard() {
    build_yoke();
    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    seed_project(project);

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    write_executable(
        &mock_bin_dir.join("cstat"),
        r#"#!/usr/bin/env bash
set -euo pipefail
count_file=".cstat-count"
count=0
if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
if [ "$count" -eq 1 ]; then
    cost=10.0
else
    cost=8.0
fi
printf '{"cstat_version":"fake","score_version":"code_complexity_cost_v0","target":".","code_complexity_cost":%s,"top_contributors":[{"kind":"function","file":"src/lib.rs","function":"hot","cost":%s}]}\n' "$cost" "$cost"
"#,
    );
    write_executable(
        &mock_bin_dir.join("omp"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"--mode rpc"* ]]; then
    echo '{"type":"ready"}'
    while IFS= read -r line; do
        id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
        if [[ "$line" == *'"type":"prompt"'* ]]; then
            printf 'STATUS: DONE\n' > .loop/notes.md
            cat > .loop/grind-gate.py <<'PY'
#!/usr/bin/env python3
import sys
sys.exit(99)
PY
            printf '{"code_complexity_cost":1.0}\n' > .loop/cstat-baseline.json
            printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
            cat <<'JSON'
{"type":"session","version":3,"id":"sess-grind-restore","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}
{"type":"agent_end","messages":[]}
JSON
            exit 0
        fi
    done
fi
printf 'STATUS: DONE\n' > .loop/notes.md
cat > .loop/grind-gate.py <<'PY'
#!/usr/bin/env python3
import sys
sys.exit(99)
PY
printf '{"code_complexity_cost":1.0}\n' > .loop/cstat-baseline.json
exit 0
"#,
    );
    let test_path = mock_path(&mock_bin_dir);

    let init = run_yoke_init_grind(&yoke, project, &test_path);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let output = Command::new(&yoke)
        .args(["run"])
        .current_dir(project)
        .env("PATH", &test_path)
        .env("GRIND_ORACLE", "true")
        .output()
        .expect("yoke run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke run should pass after restoring protected grind files. exit={:?}\nstderr:\n{}",
        output.status.code(),
        stderr
    );
}
