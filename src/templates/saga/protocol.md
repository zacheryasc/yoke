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
| `.loop/sub-plan.md` | read | The sub-plan with stages to implement. |
| `.loop/judge.md` | read | What the judge will test. Study this — knowing the test helps you pass it. |
| `.loop/notes.md` | read+write | Your scratchpad across iterations. |
| `.loop/verdict.md` | read | The judge's last verdict (from previous brute attempt). |
| `.loop/guard-results.md` | read | Guard results from the last iteration. |
| `.loop/yoke.conf` | read | Configuration. Scope rules, guards, settings. |

All paths are relative to the repository root.

## Per-Iteration Steps

1. **Read the plan** (`.loop/sub-plan.md`). Understand the full feature and all its stages.
2. **Read your notes** (`.loop/notes.md`). This is your memory — check which stage you are on, what you tried, and what you learned.
3. **Read the verdict** (`.loop/verdict.md`). If the judge previously failed your work, this contains their exact complaints. Fix what they say is broken before advancing.
4. **Read guard results** (`.loop/guard-results.md`). If non-empty, the previous iteration's guards ran. If a guard failed, fix it before advancing.
5. **Determine task**. Either fix a guard/judge failure or implement the next incomplete stage.
6. **Implement**. Make the code changes for exactly one stage.
7. **Update notes**. Write to `.loop/notes.md`:
   - Which stage you just worked on
   - What you changed and why
   - Any issues or observations for your future self
   - A `STATUS` line at the **top** of the file (see below)
8. **Exit**. Stop. Do not loop — the outer script handles iteration.

## STATUS Signaling

The first line of `.loop/notes.md` must be one of:

- `STATUS: IN_PROGRESS` — You have more work to do (stages remain, or you expect guard failures).
- `STATUS: DONE` — All stages are implemented and you believe guards will pass.

## What Happens After You Exit

1. Guards run (diff boundary check + configured guard commands).
2. If guards pass and STATUS is DONE, the plan loop ends.
3. Then the judge (a fresh Claude with zero implementation context) verifies the feature.
4. If the judge says FAIL, you get another brute attempt — your notes are preserved but STATUS is reset to IN_PROGRESS so you re-enter the plan loop with the judge's feedback.

## Rules

- **No git operations.** Do not commit, push, branch, or modify git config.
- **Do not modify `protocol.md`, `sub-plan.md`, `judge.md`, or `yoke.conf`.** These are read-only.
- **One stage per iteration.** Implement a single stage, update notes, and exit.
- **Study judge.md.** Knowing the test helps you pass it.
- **The judge's feedback is ground truth.** Fix what they say is broken.
- **Retry discipline.** If you have failed on the same issue for 3 consecutive iterations, try a fundamentally different approach.
- **Be concise in notes.** Future-you needs signal, not noise.
- **Do not waste time.** Set sane timeouts and do not lets tests run indefinitely. Do not run the full test suite before exiting, if the guard check is going to do that anyway.
