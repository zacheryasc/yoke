# Yoke Behavioral Specification

This document defines what yoke promises to its users. Every statement here is
a testable invariant over observables — files, exit codes, process behavior.
No statement references internal functions, line numbers, or implementation
details. These invariants survive refactors and rewrites.

---

## 1. The Loop

**Story:** You write a plan, a protocol, and some guards. You run `yoke run`.
An agent executes your plan iteratively. Each iteration, it reads the protocol,
does work, and updates notes. Guards check the work. When the agent writes
`STATUS: DONE` and all guards pass, yoke exits.

### Invariants

**1.1 — The spec is immutable from the agent's perspective.**
`protocol.md`, `plan.md`, and `yoke.conf` are backed up before the loop and
restored before every iteration. The agent can overwrite them during its turn,
but those changes do not persist to the next iteration.

**1.2 — Termination requires both signals.**
The loop only exits when `STATUS: DONE` appears in `notes.md` AND all guards
pass. Neither condition alone is sufficient. If guards fail but status is DONE,
the loop continues with feedback. If guards pass but status is not DONE, the
loop continues.

**1.3 — Guard feedback is visible.**
`guard-results.md` is written after every iteration. The agent sees it on its
next turn. No guard result is silently swallowed.

**1.4 — Boundary violations block guards.**
If the diff boundary check fails, all configured guards are skipped (not run).
The agent gets boundary feedback only. Guards do not run on invalid state.

**1.5 — Interrupts are clean.**
SIGINT kills the child process immediately. The loop does not exit
mid-iteration leaving partial state — it completes the signal check and exits
at the next safe point with code 130.

---

## 2. The Judge

**Story:** In brute mode, after the worker says DONE and guards pass, a
separate fresh agent (the judge) runs. It reads `judge.md`, tests the feature,
and writes `VERDICT: PASS` or `VERDICT: FAIL` to `verdict.md`. On PASS, yoke
exits successfully. On FAIL, the worker retries.

### Invariants

**2.1 — The judge is independent.**
It is a fresh agent invocation with no shared context from the worker. Its only
input is `judge.md` and the codebase state.

**2.2 — Verdict survives retries and restarts.**
`verdict.md` is never cleared automatically. On judge FAIL, the worker sees
the judge's feedback on its next iteration. On bailout, the verdict remains
on disk so the user (or agent on restart) can read why the loop failed.
Use `yoke clean` or `yoke stash` to reset.

**2.3 — Guard results survive retries and restarts.**
Same as verdict — `guard-results.md` is never cleared automatically. It
persists across brute retries and restarts so the worker sees what the
guards reported.

**2.4 — Notes status reset on retry, nothing else.**
On judge FAIL, only the first line of `notes.md` is overwritten to
`STATUS: IN_PROGRESS`. The rest of the file — the agent's prior iteration
notes — is preserved. All other files remain as-is. The worker starts with a
clean status but full context from both its own notes and the judge's verdict.

**2.5 — Bailout is exact.**
If `max-judge-failures` consecutive judge FAILs occur, yoke exits non-zero.
The count is exact — `max-judge-failures 2` means bailout on the 2nd
consecutive FAIL, not the 3rd.

**2.6 — Judge-every overrides cadence on DONE.**
If `judge-every` is configured and the worker signals DONE, the judge fires
immediately regardless of whether the iteration is on the cadence boundary.
DONE always triggers judgment.

---

## 3. The Stash

**Story:** `yoke stash` saves the current `.loop/` state. `yoke stash pop`
restores the most recent snapshot. `yoke stash checkout <hash>` restores a
specific snapshot. `yoke clean` auto-stashes before wiping.

### Invariants

**3.1 — Stash clears the working directory.**
`stash` snapshots all non-dotfile files in `.loop/`, then removes them.
After stash, `.loop/` contains only dotfile entries (like `.stash/`).
`stash` then `pop` (or `checkout`) restores the original contents — no
file is lost, truncated, or corrupted.

**3.2 — Auto-stash before destructive operations.**
Both `clean` and `checkout` auto-stash current state before modifying it. You
can always recover what was there before.

**3.3 — Mode tag is recorded.**
Each stash entry records the mode (loop/brute/saga) that was active when it
was created. This tag is preserved in the index and survives restore
operations.

**3.4 — Index is append-only.**
Stash never modifies or deletes existing index lines. New entries are appended.
The index is a history, not a mutable pointer.

**3.5 — Prefix matching is unambiguous.**
`checkout abc` matches any entry starting with `abc`. If multiple entries
match, yoke errors instead of guessing. No silent wrong restore.

---

## 4. The Saga

**Story:** Saga mode has a scoper agent that reads `specification.md`,
decomposes it into chunks, writes each chunk to `sub-plan.md`, and a brute
loop implements and verifies each chunk. When all chunks are done, the scoper
writes `STATUS: DONE` to `saga-notes.md`.

### Invariants

**4.1 — Saga completion checks saga-notes, not notes.**
The saga loop checks `saga-notes.md` for DONE. `notes.md` is local to each
brute chunk and is cleared between chunks. Checking `notes.md` would be
checking the wrong file.

**4.2 — Brute bailout triggers re-scoping, not abort.**
If brute fails `max-judge-failures` times on a chunk, control returns to the
scoper. The scoper can re-scope the same chunk differently. The saga does not
abort on a single chunk failure.

**4.3 — Sub-plan must be non-empty.**
If the scoper produces an empty `sub-plan.md`, the saga aborts. This prevents
a brute loop from running with no plan.

**4.4 — Chunk state persists and is logged.** `notes.md`, `verdict.md`, and
`guard-results.md` are NOT cleared between chunks. The scoper can read why
the previous chunk failed or succeeded. Before each chunk, the contents of
`notes.md` are appended to `saga-log.md`. Use `yoke clean` or `yoke stash`
for a full reset.

**4.5 — Saga log is append-only.** `saga-log.md` accumulates the worker's
notes from every completed chunk. It is never cleared or truncated during a
saga run. Each entry is labeled with its chunk number.

---

## 5. Config

**Story:** `yoke.conf` defines the rules of the loop — what files are
protected, what guards run, how the judge behaves. It is parsed once at
startup and applied consistently throughout the run.

### Invariants

**5.1 — Valid configs parse.**
Every legal combination of directives parses without error.

**5.2 — Invalid configs fail loudly.**
Unknown directives, malformed values, and missing required fields produce clear
errors — not silent defaults.

**5.3 — Scope rules resolve most-specific-wins.**
If `allow src/` and `no-modify src/main.rs` are both configured, `src/main.rs`
is protected and `src/other.rs` is allowed. Longer prefix wins.

**5.4 — Guard-after requires its periodic.**
A `guard-after` referencing a periodic that does not exist is a config error,
not a silent no-op.

**5.5 — Hooks are fire-and-forget.**
A `hook` command that exits non-zero or fails to execute produces a warning on
stderr but does not affect the loop's exit code, guard evaluation, or iteration
flow. Hook output is never written to any file the agent reads.

---

## 6. Mode Switching

**Story:** You can switch between loop, brute, and saga without losing
progress. Each mode's state is snapshotted when you leave it and restored
when you return.

### Invariants

**6.1 — Mode switch stashes current state.**
Switching from mode A to B stashes all of A's files via `yoke stash`. The
stash entry is tagged with mode A. Current state is always recoverable.

**6.2 — Mode switch always fresh-inits.**
After stashing, the target mode is initialized with fresh template files.
Previous sessions are not auto-restored. Use `yoke stash checkout` to
restore a prior session.
