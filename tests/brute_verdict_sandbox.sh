#!/usr/bin/env bash
# Integration test: brute-mode verdict preservation through Docker sandbox.
#
# Validates that yoke correctly constructs the docker command, volume mounts
# work (container writes to .loop/ are visible on host), and verdict.md
# survives brute retries through the containerized path.
#
# Prerequisites:
#   - docker daemon running
#   - omp-sandbox:latest image available
#   - cargo (to build yoke)
#
# Usage:
#   bash tests/brute_verdict_sandbox.sh

set -euo pipefail

TEST_IMAGE="yoke-test-verdict:latest"
TMPDIR_ROOT=""

cleanup() {
    if [[ -n "$TMPDIR_ROOT" && -d "$TMPDIR_ROOT" ]]; then
        rm -rf "$TMPDIR_ROOT"
    fi
    docker rmi "$TEST_IMAGE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── Prerequisites ──

if ! command -v docker &>/dev/null; then
    echo "SKIP: docker not found on PATH"
    exit 0
fi

if ! docker image inspect omp-sandbox:latest &>/dev/null; then
    echo "SKIP: omp-sandbox:latest image not found"
    exit 0
fi

if ! command -v cargo &>/dev/null; then
    echo "SKIP: cargo not found on PATH"
    exit 0
fi

# ── Build yoke ──

echo "Building yoke..."
cargo build --quiet

YOKE_BIN="$(pwd)/target/debug/yoke"
if [[ ! -x "$YOKE_BIN" ]]; then
    echo "FAIL: yoke binary not found at $YOKE_BIN"
    exit 1
fi

# ── Create temp workspace ──

TMPDIR_ROOT="$(mktemp -d)"
PROJECT="$TMPDIR_ROOT/project"
mkdir -p "$PROJECT"

# ── Write mock-omp script ──

cat > "$TMPDIR_ROOT/mock-omp" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail

# Extract the prompt from -p argument
PROMPT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p) PROMPT="$2"; shift 2 ;;
        *)  shift ;;
    esac
done

if echo "$PROMPT" | grep -q "protocol.md"; then
    # ── Agent mode ──
    COUNTER_FILE=".loop/.agent-calls"
    N=0
    if [[ -f "$COUNTER_FILE" ]]; then
        N=$(cat "$COUNTER_FILE")
    fi
    N=$((N + 1))
    echo "$N" > "$COUNTER_FILE"

    # Witness: snapshot of verdict.md at the moment the agent runs
    cp .loop/verdict.md ".loop/.witness-${N}"

    # Write STATUS: DONE so plan loop exits
    printf 'STATUS: DONE\n\n## Stage 1 — Minimal\nDone.\n' > .loop/notes.md

elif echo "$PROMPT" | grep -q "judge.md"; then
    # ── Judge mode ──
    COUNTER_FILE=".loop/.judge-calls"
    N=0
    if [[ -f "$COUNTER_FILE" ]]; then
        N=$(cat "$COUNTER_FILE")
    fi
    N=$((N + 1))
    echo "$N" > "$COUNTER_FILE"

    if [[ "$N" -eq 1 ]]; then
        printf 'VERDICT: FAIL\nFAILURE-ID: step-counter:not-incremented\n\nFeature is broken — step counter never increments.' > .loop/verdict.md
    else
        printf 'VERDICT: PASS\n\nAll checks passed.' > .loop/verdict.md
    fi
fi

exit 0
MOCK
chmod +x "$TMPDIR_ROOT/mock-omp"

# ── Build test Docker image ──

echo "Building test image $TEST_IMAGE..."
docker build -t "$TEST_IMAGE" -f- "$TMPDIR_ROOT" <<'DOCKERFILE'
FROM omp-sandbox:latest
COPY --chmod=755 mock-omp /usr/local/bin/omp
DOCKERFILE

# ── Set up project directory ──

LOOP_DIR="$PROJECT/.loop"
mkdir -p "$LOOP_DIR"

cat > "$LOOP_DIR/protocol.md" <<'EOF'
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
EOF

cat > "$LOOP_DIR/plan.md" <<'EOF'
## Stage 1 — Minimal

Implement the feature.
EOF

cat > "$LOOP_DIR/judge.md" <<'EOF'
# Judge

Verify the feature works.

## Verdict

Write VERDICT: PASS or VERDICT: FAIL to .loop/verdict.md, with a stable
FAILURE-ID on the next line for FAIL.
EOF

cat > "$LOOP_DIR/yoke.conf" <<EOF
image $TEST_IMAGE
allow .
EOF

touch "$LOOP_DIR/notes.md"
touch "$LOOP_DIR/verdict.md"
touch "$LOOP_DIR/guard-results.md"

# ── Git init ──

git -C "$PROJECT" init --quiet
echo "seed" > "$PROJECT/dummy.txt"
git -C "$PROJECT" add dummy.txt
GIT_CONFIG_NOSYSTEM=1 \
  git -C "$PROJECT" \
    -c user.name=test \
    -c user.email=test@test \
    commit --quiet -m "init"

# ── Run yoke (sandboxed — no --no-sandbox flag) ──

echo "Running yoke in sandbox mode..."
set +e
OUTPUT=$(cd "$PROJECT" && "$YOKE_BIN" run 2>&1)
YOKE_EXIT=$?
set -e

# ── Assertions ──

PASS=true

# 1. witness-1 should be empty: no verdict exists before first agent run
if [[ ! -f "$LOOP_DIR/.witness-1" ]]; then
    echo "FAIL: .witness-1 does not exist (agent was never called)"
    PASS=false
else
    WITNESS_1=$(cat "$LOOP_DIR/.witness-1")
    if [[ -n "$WITNESS_1" ]]; then
        echo "FAIL: witness-1 should be empty (no prior verdict), got: '$WITNESS_1'"
        PASS=false
    else
        echo "OK: witness-1 is empty (no prior verdict)"
    fi
fi

# 2. witness-2 must contain VERDICT: FAIL — agent saw judge's feedback
if [[ ! -f "$LOOP_DIR/.witness-2" ]]; then
    echo "FAIL: .witness-2 does not exist (agent was not called a second time)"
    echo "  Agent calls: $(cat "$LOOP_DIR/.agent-calls" 2>/dev/null || echo 'N/A')"
    echo "  Judge calls: $(cat "$LOOP_DIR/.judge-calls" 2>/dev/null || echo 'N/A')"
    echo "  Yoke output:"
    echo "$OUTPUT" | sed 's/^/    /'
    PASS=false
else
    WITNESS_2=$(cat "$LOOP_DIR/.witness-2")
    if echo "$WITNESS_2" | grep -q "VERDICT: FAIL"; then
        echo "OK: witness-2 contains 'VERDICT: FAIL' (agent saw judge feedback)"
    else
        echo "FAIL: witness-2 should contain 'VERDICT: FAIL', got: '$WITNESS_2'"
        PASS=false
    fi
fi

# 3. yoke exits 0 — judge eventually said PASS
if [[ "$YOKE_EXIT" -eq 0 ]]; then
    echo "OK: yoke exited 0 (judge said PASS)"
else
    echo "FAIL: yoke exited $YOKE_EXIT (expected 0)"
    echo "  Yoke output:"
    echo "$OUTPUT" | sed 's/^/    /'
    PASS=false
fi

# ── Summary ──

if [[ "$PASS" == true ]]; then
    echo ""
    echo "PASS: brute verdict preserved through Docker sandbox"
    exit 0
else
    echo ""
    echo "FAIL: one or more assertions failed"
    exit 1
fi
