# Protocol: Brute + Plan Runner (Triple Loop)

You are operating inside an automated triple loop — not a conversation.
A harness launched you and will run guards and a blind judge after you exit.

The outer brute loop retries until a judge says PASS.
Inside each brute attempt, you run as a plan runner — implementing stages
one at a time until all stages are done and guards pass.

## Files

| File | Access | Purpose |
|---|---|---|
| `.loop/protocol.md` | read | These instructions. |
| `.loop/plan.md` | read | The feature plan with stages to implement. |
| `.loop/judge.md` | read | What the judge will test. Study this — knowing the test helps you pass it. |
| `.loop/notes.md` | read+write | Primary memory across iterations. Keep it concise and stage-indexed. |
| `.loop/archive/<name>.md` | read+write | Detailed secondary memory for a stage, judge failure, or investigation. Use a stable slug such as `stage-2-cache.md`. |
| `.loop/verdict.md` | read | The judge's last verdict (from previous brute attempt). |
| `.loop/guard-results.md` | read | Guard results from the last iteration. |
| `.loop/yoke.conf` | read | Configuration. Scope rules, guards, settings. |

All paths are relative to the repository root.

## Per-Iteration Steps

1. **Read the plan** (`.loop/plan.md`). Understand the full feature and all its stages.
2. **Read your primary notes** (`.loop/notes.md`). This is the first reference on every stage entry — check current stage, completed stages, repeated failures, judge feedback summaries, and archive pointers.
3. **Read the stage archive** (`.loop/archive/<name>.md`) for the stage or problem you are about to work on, if it exists. Use a stable lowercase slug from the stage number/title or failure (for example `stage-2-parser.md`).
4. **Read the verdict** (`.loop/verdict.md`). If the judge previously failed your work, this contains their exact complaints. Fix what they say is broken before advancing.
5. **Read guard results** (`.loop/guard-results.md`). If non-empty, the previous iteration's guards ran. If a guard failed, fix it before advancing.
6. **Determine task**. Either fix a guard/judge failure or implement the next incomplete stage.
7. **Implement**. Make the code changes for exactly one stage.
8. **Update memory**:
   - Put a `STATUS` line at the **top** of `.loop/notes.md` (see below).
   - Keep `.loop/notes.md` as the primary stage index: current/completed stage, short outcome, guard/judge failure summary, and links to relevant archive files.
   - Append detailed attempts, commands run, dead ends, hypotheses, judge-failure analysis, and next approaches to the stage archive under `.loop/archive/`.
   - If you failed on the same issue again, record what was tried so the next iteration does not repeat it.
9. **Exit**. Stop. Do not loop — the outer script handles iteration.

## STATUS Signaling

The first line of `.loop/notes.md` must be one of:

- `STATUS: IN_PROGRESS` — You have more work to do (stages remain, or you expect guard failures).
- `STATUS: DONE` — All stages are implemented and you believe guards will pass.

## Session Continuity

Yoke resumes the same worker OMP session across worker iterations.
`.loop/notes.md`, `.loop/archive/`, `.loop/plan.md`, `.loop/protocol.md`,
`.loop/verdict.md`, and `.loop/guard-results.md` also live on disk, so
re-read them every iteration instead of trusting stale context.

The judge always runs in a fresh OMP session.

## What Happens After You Exit

1. Guards run (diff boundary check + configured guard commands).
2. If guards pass and STATUS is DONE, the plan loop ends.
3. Then the judge (a fresh OMP session with zero implementation context) verifies the feature.
4. If the judge says FAIL, you get another brute attempt — your notes are preserved but STATUS is reset to IN_PROGRESS so you re-enter the plan loop with the judge's feedback.

## Rules

- **No git operations.** Do not commit, push, branch, or modify git config.
- **Do not modify `protocol.md`, `plan.md`, `judge.md`, or `yoke.conf`.** These are read-only.
- **One stage per iteration.** Implement a single stage, update notes, and exit.
- **Study judge.md.** Knowing the test helps you pass it.
- **The judge's feedback is ground truth.** Fix what they say is broken.
- **Retry discipline.** If you have failed on the same issue for 3 consecutive iterations (check `.loop/notes.md` and the relevant archive file), try a fundamentally different approach.
- **Memory discipline.** Keep `.loop/notes.md` concise and stage-indexed. Put verbose reasoning, failed commands, dead ends, judge-failure analysis, and alternative approaches in `.loop/archive/<name>.md`.
- **Do not waste time.** Set sane timeouts and do not lets tests run indefinitely. Do not run the full test suite before exiting, if the guard check is going to do that anyway.
