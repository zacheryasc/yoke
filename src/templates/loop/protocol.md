# Protocol: Automated CI Loop

You are operating inside an automated loop — not a conversation. A bash script launched you, and will run guard checks after you exit. You do not interact with a human during this session.

## Files

| File | You can | Purpose |
|------|---------|---------|
| `.loop/protocol.md` | read | This document. Your instructions. |
| `.loop/plan.md` | read | The feature plan. Stages to implement. |
| `.loop/notes.md` | read + write | Your scratchpad. Persists across iterations. |
| `.loop/guard-results.md` | read | Guard results from the last iteration. |
| `.loop/yoke.conf` | read | Loop configuration. Scope rules, guards, settings. |

All paths are relative to the repository root.

## Per-Iteration Steps

1. **Read the plan** (`.loop/plan.md`). Understand the full feature and all its stages.
2. **Read your notes** (`.loop/notes.md`). This is your memory across iterations — check which stage you are on, what you tried, and what you learned.
3. **Read guard results** (`.loop/guard-results.md`). If it exists and is non-empty, the previous iteration's guards ran. Look for failures. If a guard failed, your priority is fixing the failure before advancing to a new stage.
4. **Determine task**. Either fix a guard failure (if any) or implement the next incomplete stage from the plan.
5. **Implement**. Make the code changes for exactly one stage. Work in the repository's working tree.
6. **Update notes**. Write to `.loop/notes.md`:
   - Which stage you just worked on
   - What you changed and why
   - Any issues or observations for your future self
   - A `STATUS` line at the **top** of the file (see below)
7. **Exit**. Stop. Do not loop — the outer script handles iteration.

## STATUS Signaling

The first line of `.loop/notes.md` must be one of:

- `STATUS: IN_PROGRESS` — You have more work to do (stages remain, or you expect guard failures).
- `STATUS: DONE` — All stages in the plan are implemented and you believe guards will pass.

The outer loop reads this line. It exits only when `STATUS: DONE` **and** all guards pass.

## Session Continuity

Yoke resumes the same worker OMP session across iterations. `.loop/notes.md`,
`.loop/plan.md`, `.loop/protocol.md`, and `.loop/guard-results.md` also live on
disk, so re-read them every iteration instead of trusting stale context.

## What the Guards Check

After you exit, the outer loop runs guards defined in `.loop/yoke.conf`.

1. **Diff boundary check** — Always runs first. Verifies every file you changed
   or created is within the scope rules defined in `.loop/yoke.conf`. The rules:
   - `allow PREFIX` — anything goes: add, modify, delete.
   - `add-only PREFIX` — may only add lines; no removing existing lines.
   - `no-modify PREFIX` — zero modifications allowed.
   - No matching rule — change is denied.
   - Most-specific (longest) prefix wins when rules overlap.
   If the boundary check fails, all subsequent guards are skipped.
2. **Configured guards** — Read the `guard` lines in `.loop/yoke.conf` to see
   what commands run. Guards execute in order, fail-fast (first failure skips
   the rest).

You may run any commands you find useful during implementation.

## Rules

- **No git operations.** Do not commit, push, branch, or modify git config. The outer loop owns git.
- **Do not modify `protocol.md`, `plan.md`, or `yoke.conf`.** These are read-only to you.
- **One stage per iteration.** Implement a single stage, update notes, and exit. Do not attempt multiple stages.
- **Retry discipline.** If you have failed on the same issue for 3 consecutive iterations (check your notes), try a fundamentally different approach. Do not repeat the same fix.
- **Be concise in notes.** Future-you needs signal, not noise. Record what matters: what stage, what changed, what broke, what to try next.
- **Do not waste time.** Set sane timeouts and do not lets tests run indefinitely. Do not run the full test suite before exiting, if the guard check is going to do that anyway.
