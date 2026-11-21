# Yoke Design

This document records the current shape of Yoke and the planned OMP
integration. It is meant to steer implementation, not to prescribe exact
function names or line-level changes.

## 1. Current state

### Brute is the only mode

Yoke now exposes one loop: brute.

- `yoke init` creates the brute profile:
  `yoke.conf`, `protocol.md`, `briefing.md`, `plan.md`, `judge.md`,
  `notes.md`, `verdict.md`, `guard-results.md`, and `archive/`.
- `yoke run` always runs the brute loop: the plan loop runs worker iterations
  until `STATUS: DONE` and guards pass, then a fresh judge decides PASS/FAIL.
- Saga, grind, and the standalone loop profile have been removed:
  templates, mode detection/switching, `grind-passes`, and the cstat gate are
  gone.

### Per-run artifacts

Every run writes a self-contained directory:

```text
.loop/runs/<run-id>/
  run.json              manifest: outcome, iterations, VCS pointers
  summary.md            human entry point
  snapshots/            plan/judge/protocol/config/notes at run start
  iterations/0001/
    meta.json           timings, guard state, branch/HEAD
    diff.patch          cumulative diff from the run's base commit
    diffstat.txt
    guard-results.md
    notes.md
    verdict.md
  judge/attempt-0001.md standalone judge verdicts
  final.diff            cumulative diff at run end
  final-verdict.md
  final-notes.md
  archive/              copy of .loop/archive at run end
```

Properties:

- Diffs are captured through a temporary Git index. The real index is never
  staged, and the harness never commits or moves the branch.
- The manifest records the base branch and commit, plus the current
  branch/HEAD at each capture, so a patch points back to where it was made.
- `yoke clean` resets working files but preserves `.loop/runs/`.
- Artifacts are the durable record. They are not the live monitoring channel.

## 2. Goal: drive Yoke from an OMP session

An outer OMP agent should be able to:

1. Prepare the loop: write or edit `plan.md`, `judge.md`, notes, and config.
2. Start `yoke run` itself.
3. Live-monitor progress through compact summaries.
4. Steer or pause the inner worker without restarting the run.
5. Stop and receive the final result.

The outer agent must not consume the raw inner-worker stream. Summaries are
the monitoring surface; artifacts are the deep-inspection surface.

## 3. Two streams

### Main worker stream

OMP emits JSONL on stdout. Yoke parses it into discrete **blocks**:

- tool calls
- thinking blocks
- regular output messages

This is the raw stream. It is not the observer's stream. Today it is only
retained when `log-dir` is configured; summaries must not depend on that.

### Summary stream

A separate, parallel NDJSON stream carries summary records to the OMP tool.
It is:

- never mixed into the main worker stream;
- never appended to run artifacts;
- consumed live by the observing agent;
- freeform in content — the cheap model simply summarizes the raw block batch
  as well as it can, with no required schema.

The exact transport is an implementation choice (for example, a dedicated file
descriptor or pipe from `yoke run` to the extension). It must stay separate
from stdout/stderr and from on-disk artifacts.

## 4. Summary cadence

Summaries are block-driven, not time-driven.

- Count blocks as they are parsed.
- Every **X blocks**, enqueue the raw blocks accumulated since the previous
  summary batch.
- A configured cheap model summarizes each batch.
- X is the primary tuning knob:
  - larger X: less summary traffic, less outer-agent context, less chance of
    micromanagement;
  - smaller X: finer-grained steering.
- Start with a reasonable X and tune it until the behavior proves itself.

There is no wall-clock timer and no background "is a summarizer running"
scheduler. A simple queue drains summary batches in order.

The cheap model should be configured explicitly (model name plus any thinking
setting). Summarization must not use the expensive monitoring model.

## 5. Backpressure and pause

If summary work falls behind, the backlog is measured. Past a configured
threshold, Yoke pauses the worker at the next block boundary, then resumes when
the queue has drained.

Use OMP's existing session block/pause mechanism where possible. Do not build a
second, competing pause system. Verify the available mechanism before choosing
a fallback; the fallback is a lightweight block-boundary stop of the inner
worker while Yoke stays alive to accept control commands.

Requirements:

- No summarizer work is dropped.
- The worker is paused at a block boundary, not killed or restarted.
- The inner OMP session and all state survive the pause.
- Resume continues from the same point.
- The pause is fine-grained and light-touch, so steering is possible at block
  resolution.

Backpressure policy belongs in Yoke core, because core owns the block counter,
the summary queue, and the pause point.

## 6. Explicit pause

Pause is also a user/observer action, not only a backpressure response.

- `pause` freezes the run at the next block boundary.
- The inner worker/session and filesystem state are preserved.
- `resume` continues without restarting.
- While paused, `.loop/` is editable.
- There is no approval gate or change-approval flow.
- On resume, Yoke re-snapshots protected files so paused edits to
  `plan.md`, `judge.md`, `protocol.md`, and `yoke.conf` are not clobbered by the
  normal per-iteration restore.

## 7. OMP tool surface

The extension exposes a `yoke` tool with exactly:

| Action | Behavior |
|---|---|
| `start` | Spawn `yoke run` in the project cwd; return run id and artifact location. |
| `pause` | Freeze the run at the next block boundary. |
| `resume` | Continue a paused run in place. |
| `inject` | Send steering text to the inner worker. |
| `stop` | End the run gracefully and return the final result. |

Properties:

- Summaries arrive live on the side stream; there is no polling action for
  them.
- No `wait`, `status`, or `result` action.
- The final result is returned automatically when the run ends, including via
  `stop`.
- The child is owned by the OMP session. On session shutdown, stop it
  gracefully first, then force termination if needed.
- No `run-state.json`, no `yoke status`, no run-state file, no summary files in
  artifacts, no attention heuristics, and no proactive heuristic notifications.

## 8. File ownership

- The outer agent prepares `.loop/` before `start`.
- `notes.md` and `archive/` are live-editable during a run.
- `plan.md`, `judge.md`, `protocol.md`, and `yoke.conf` are restored every
  iteration and should not be edited mid-run unless the run is paused.
- Paused edits are allowed and must survive resume via re-snapshotting.
- Deep inspection is ordinary file reading: the observer can inspect
  `.loop/runs/<run-id>/` with its normal tools.

## 9. Non-goals

- No saga or grind revival.
- No automatic Git commits; patches are artifacts only.
- No raw-stream persistence requirement for summaries.
- No structured summary schema.
- No attention/health scoring layer.
- No separate live daemon or status file.

## 10. Suggested implementation order

1. **Core block counter and summary queue**
   - Count parsed blocks.
   - Every X blocks, enqueue the raw batch.
   - Invoke the configured cheap model.
   - Emit summaries on the side stream only.
2. **Pause/resume in core**
   - Block-boundary pause.
   - Preserve the inner worker/session.
   - Re-snapshot protected files on resume.
3. **Backpressure**
   - Queue depth threshold.
   - Automatic pause/resume using the same pause path.
4. **OMP extension**
   - `start`, `pause`, `resume`, `inject`, `stop`.
   - Consume the summary side stream live.
   - Session-owned lifecycle and graceful shutdown.
5. **End-to-end verification**
   - Real outer-OMP-to-yoke run with summaries, a pause/edit/resume cycle,
     steering, and a final result.

## 11. Open implementation questions

- Exact transport for the summary side stream.
- Exact OMP session block/pause API; use it if it exists and fits.
- How to invoke the cheap summarizer without tools and without touching the
  worker's session.
- Queue-depth threshold and how it is configured.
- Initial value of X and initial summary model.

These are implementation details; the behavior above is the contract.

## 12. Implementation notes

The contract above is implemented with these choices:

- **Summary transport:** `yoke run --summary-fd N` (or `YOKE_SUMMARY_FD`)
  writes NDJSON to an inherited descriptor. The OMP extension passes fd 3.
  Records are `run_start`, `summary`, and `run_end`; the descriptor is
  close-on-exec so summarizer children do not inherit it. No summary bytes go
  to stdout, stderr, or `.loop/runs/`.
- **Block boundary:** a completed text block, thinking block, or executed tool
  call (`text_end`, `thinking_end`, `tool_execution_end`). `toolcall_end` is
  grouped with the tool execution it describes.
- **Cadence and queue:** `summary-every` defaults to 12 blocks, and
  `summary-queue-limit` defaults to 4 outstanding batches. Batch depth counts
  the batch currently being summarized. Config keys are `summary-every`,
  `summary-model`, `summary-thinking`, and `summary-queue-limit`.
- **Summarizer:** a separate `omp --mode text --no-session --no-tools`
  invocation using `summary-model` and `summary-thinking`, never the worker
  model or session.
- **Pause:** OMP RPC has no pause command (only `abort`/`resume`, which cancel
  the active turn). Yoke therefore stops the RPC worker's process group with
  `SIGSTOP` after a block boundary and continues it with `SIGCONT`. This also
  freezes tool children. The process, session, pipes, and filesystem state
  survive. On resume, protected files are re-snapshotted into the iteration
  backup before the next restore. With `image` sandboxing the docker client is
  frozen; container-side processes may continue until the client resumes.
- **Control surface:** the existing Unix socket gains `pause`, `resume`, and
  `stop` alongside `inject`; the CLI exposes the same commands. The extension
  in `extensions/yoke.ts` exposes the five-action `yoke` tool and consumes the
  side stream live; final results come from `run.json` when the child exits.
  Set `YOKE_BIN` when `yoke` is not on the supervising session's `PATH`.
- **Verification:** `tests/summary_stream.rs` covers cadence, side-stream
  isolation, backpressure pause/resume, and no dropped batches;
  `tests/pause_resume.rs` covers block-boundary freeze, same-process resume,
  paused steering, and paused edits surviving restore. The extension loads in
  a real OMP RPC session, its `yoke` tool appears in the tool prompt, and a
  full outer-OMP start/pause/edit/resume/inject/stop cycle completed with the
  paused edit surviving into the final artifacts.
