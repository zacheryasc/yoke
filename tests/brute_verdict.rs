//! Integration test: brute-mode verdict preservation.
//!
//! Verifies that after a judge writes VERDICT: FAIL, the verdict.md content
//! survives into the next brute iteration so the agent can read the feedback.
//!
//! Uses a mock `omp` bash script to simulate both agent and judge,
//! recording what the agent sees in verdict.md at each invocation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// Path to the yoke binary built by cargo for this test target.
fn yoke_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_yoke"))
}

/// Minimal brute protocol — just enough for the plan runner to work.
const PROTOCOL: &str = "\
# Protocol

You are inside an automated loop.

## Files

| File | Access | Purpose |
|---|---|---|
| `.loop/protocol.md` | read | These instructions. |
| `.loop/plan.md` | read | The feature plan. |
| `.loop/judge.md` | read | What the judge tests. |
| `.loop/notes.md` | read+write | Your scratchpad. |
| `.loop/verdict.md` | read | Previous judge verdict. |
| `.loop/guard-results.md` | read | Guard results. |
| `.loop/yoke.conf` | read | Configuration. |

## Per-Iteration Steps

1. Read plan.
2. Read notes.
3. Read verdict.
4. Implement one stage.
5. Update notes with STATUS line.
6. Exit.

## STATUS Signaling

First line of notes.md: `STATUS: IN_PROGRESS` or `STATUS: DONE`.
";

const PLAN: &str = "\
## Stage 1 — Minimal

Implement the feature.
";

const JUDGE: &str = "\
# Judge

Verify the feature works.

## Verdict

Write VERDICT: PASS or VERDICT: FAIL to .loop/verdict.md.
";

const CONF: &str = "\
allow .
";

/// Mock OMP script that distinguishes agent vs judge by the -p prompt.
///
/// Agent mode (prompt contains "protocol.md"):
///   - Increments .loop/.agent-calls counter
///   - Copies verdict.md to .loop/.witness-N
///   - Writes STATUS: DONE to notes.md
///
/// Judge mode (prompt contains "judge.md"):
///   - Increments .loop/.judge-calls counter
///   - Call 1: writes VERDICT: FAIL + feedback to verdict.md
///   - Call 2+: writes VERDICT: PASS to verdict.md
const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail

{
    echo "CALL"
    for arg in "$@"; do
        echo "$arg"
    done
} >> .loop/.omp-argv

MODE=""
for arg in "$@"; do
    if [[ "$arg" == "rpc" ]]; then MODE="rpc"; fi
done

if [[ "$MODE" == "rpc" ]]; then
    echo '{"type":"ready"}'
    while IFS= read -r line; do
        id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
        if [[ "$line" == *'"type":"prompt"'* ]]; then
            COUNTER_FILE=".loop/.agent-calls"
            N=0
            if [[ -f "$COUNTER_FILE" ]]; then
                N=$(cat "$COUNTER_FILE")
            fi
            N=$((N + 1))
            echo "$N" > "$COUNTER_FILE"
            cp .loop/verdict.md ".loop/.witness-${N}"
            printf 'STATUS: DONE\n\n## Stage 1 — Minimal\nDone.\n' > .loop/notes.md
            printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
            cat <<'JSON'
{"type":"session","version":3,"id":"sess-brute-worker","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}
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

# Extract the prompt from -p argument for fresh judge/scoper invocations.
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "protocol.md"; then
    COUNTER_FILE=".loop/.agent-calls"
    N=0
    if [[ -f "$COUNTER_FILE" ]]; then
        N=$(cat "$COUNTER_FILE")
    fi
    N=$((N + 1))
    echo "$N" > "$COUNTER_FILE"
    cp .loop/verdict.md ".loop/.witness-${N}"
    printf 'STATUS: DONE\n\n## Stage 1 — Minimal\nDone.\n' > .loop/notes.md
elif echo "$PROMPT" | grep -q "judge.md"; then
    COUNTER_FILE=".loop/.judge-calls"
    N=0
    if [[ -f "$COUNTER_FILE" ]]; then
        N=$(cat "$COUNTER_FILE")
    fi
    N=$((N + 1))
    echo "$N" > "$COUNTER_FILE"
    if [[ "$N" -eq 1 ]]; then
        printf 'VERDICT: FAIL\n\nFeature is broken — step counter never increments.' > .loop/verdict.md
    else
        printf 'VERDICT: PASS\n\nAll checks passed.' > .loop/verdict.md
    fi
fi

exit 0
"#;

#[test]
fn brute_verdict_preserved_across_iterations() {
    // Build yoke first
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    assert!(yoke.exists(), "yoke binary not found at {:?}", yoke);

    // Create a temp directory for the project
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    // Set up .loop/ directory with required files
    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).expect("create .loop");

    fs::write(loop_dir.join("protocol.md"), PROTOCOL).unwrap();
    fs::write(loop_dir.join("plan.md"), PLAN).unwrap();
    fs::write(loop_dir.join("judge.md"), JUDGE).unwrap();
    fs::write(loop_dir.join("yoke.conf"), CONF).unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("verdict.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    // Set up mock OMP script on PATH
    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).expect("create mock-bin");

    let mock_omp_path = mock_bin_dir.join("omp");
    fs::write(&mock_omp_path, MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp_path, fs::Permissions::from_mode(0o755)).unwrap();

    let legacy_bin_path = mock_bin_dir.join(["cl", "aude"].concat());
    fs::write(
        &legacy_bin_path,
        "#!/usr/bin/env bash\necho 'legacy agent must not be invoked' >&2\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&legacy_bin_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Set up git repo (boundary checker needs `git diff HEAD` to work)
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
            .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {}", args, e));
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

    // Build PATH: mock-bin first so our mock OMP shadows the real one
    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);

    // Run yoke
    let output = Command::new(&yoke)
        .args(["run", "--no-sandbox"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("failed to run yoke");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // ── Assertions ──
    let argv = fs::read_to_string(loop_dir.join(".omp-argv")).expect("mock omp argv");
    assert!(
        argv.contains("--mode\nrpc")
            && argv.contains("--auto-approve")
            && argv.contains("--mode\njson")
            && argv.contains("-p\nRead .loop/judge.md"),
        "main worker should use RPC and judge should still use JSON print mode.\nargv:\n{}",
        argv
    );

    // 1. witness-1 should be empty: no verdict exists before first agent run
    let witness_1 = fs::read_to_string(loop_dir.join(".witness-1"))
        .expect(".witness-1 must exist (agent was called at least once)");
    assert!(
        witness_1.is_empty(),
        "witness-1 should be empty (no prior verdict), got: {:?}",
        witness_1
    );

    // 2. witness-2 must contain VERDICT: FAIL — agent saw judge's feedback
    let witness_2_path = loop_dir.join(".witness-2");
    assert!(
        witness_2_path.exists(),
        "witness-2 must exist (agent should have been called a second time).\n\
         Agent calls: {:?}\nJudge calls: {:?}\nStderr:\n{}",
        fs::read_to_string(loop_dir.join(".agent-calls")).ok(),
        fs::read_to_string(loop_dir.join(".judge-calls")).ok(),
        stderr,
    );
    let witness_2 = fs::read_to_string(&witness_2_path).unwrap();
    assert!(
        witness_2.contains("VERDICT: FAIL"),
        "witness-2 must contain 'VERDICT: FAIL' (agent should see judge feedback on retry).\n\
         Got: {:?}\nStderr:\n{}",
        witness_2,
        stderr,
    );

    // 3. yoke exits 0 — judge eventually said PASS
    assert!(
        output.status.success(),
        "yoke should exit 0 (judge said PASS on second attempt).\n\
         Exit code: {:?}\nStderr:\n{}",
        output.status.code(),
        stderr,
    );
}
