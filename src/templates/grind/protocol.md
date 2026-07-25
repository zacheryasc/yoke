# Protocol: Grind

You are operating inside an automated grind loop — not a conversation. A harness launched you, and will run the grind gate after you exit.

## Files

| File | You can | Purpose |
|------|---------|---------|
| `.loop/protocol.md` | read | This document. Your instructions. |
| `.loop/notes.md` | read + write | Your scratchpad across iterations. |
| `.loop/guard-results.md` | read | The previous grind gate output. |
| `.loop/cstat-baseline.json` | read | Baseline `cstat --path . --json scorecard` captured before edits. |
| `.loop/cstat-current.json` | read | Last gate's current scorecard JSON, when present. |
| `.loop/grind-gate.py` | read | The guard harness. It runs the behavior oracle, optional benchmark, and cstat comparison. |
| `.loop/yoke.conf` | read | Scope rules and guard settings. |

All paths are relative to the repository root.

## Objective

Drive `code_complexity_cost` down aggressively while preserving observable behavior.

The gate passes only when the behavior oracle succeeds and current `cstat --path . --json scorecard` reports a lower `code_complexity_cost` than `.loop/cstat-baseline.json`.

## Using cstat

Primary objective command:

```text
cstat --path . scorecard --top 20
```

Machine-readable gate command:

```text
cstat --path . --json scorecard
```

Global options from `cstat help`:

- `--path <PATH>` — Rust project directory or Rust source file to analyze; default is `.`.
- `--json` — output structured JSON instead of human-readable text.
- `--verbose` — print educational explanations for each section.
- `--no-color` — disable colored output.

Commands from `cstat help`:

- `scorecard` — deterministic structural code complexity scorecard. This is the objective. Lower `code_complexity_cost` is cleaner. Use `--top <N>` to show the highest contributors.
- `branching` — function decision/path complexity rankings. Use it to simplify high-branching functions and collapse duplicated control flow.
- `signature` — function API boundary complexity rankings. Use it to reduce public surface, parameter sprawl, generic noise, and unnecessary result shapes.
- `span` — function implementation span rankings. Use it to find long functions that should be deleted, flattened, or split only when splitting reduces real complexity.
- `deps` — module dependency connectome. Use it to remove coupling, merge needless modules, or move code when it reduces dependency pressure.
- `dead-code` — static dead-code candidates. Use it to delete unused items, stale exports, and obsolete branches.
- `test-reachability` — static test/benchmark reachability. Use it to understand what behavior is protected before cutting.
- `call-trace` — tree view of calls from the Cargo binary entrypoint or `--entry <PATH::FUNCTION>`. Use it to understand runtime paths before deleting or moving code.
- `coverage` — dynamic line and branch coverage from Rust source-based coverage data. Use `--no-run` with `CSTAT_LLVM_COV_EXPORT_JSON` when parsing existing coverage data.
- `cluster` — value-cluster transcript from AST def-use structure. Use it to find code that belongs together or abstractions that should collapse.
- `loc` — lines-of-code size-shape analysis. Use it to find file concentration and large generated-looking surfaces.
- `symbols` — Rust AST symbol counts by kind, including trait impl blocks. Use it to find abstraction surface and symbol sprawl.

## Per-Iteration Steps

1. Read `.loop/notes.md`, `.loop/guard-results.md`, `.loop/cstat-baseline.json`, and `.loop/cstat-current.json` if it exists. If the previous gate failed on behavior, benchmark, boundary, or invalid cstat output, repair that first.
2. Run `cstat --path . scorecard --top 20`. Compare current cost to the baseline and identify the biggest contributors.
3. Use the relevant cstat diagnostics above. Do not sample one random command; pick the commands that explain the top contributors and the scorecard dimensions they affect.
4. Push a coherent cleanup batch. Do not stop after one simplification. Delete dead code, collapse unnecessary abstractions, simplify branching, reduce signatures, shrink spans, and cut coupling while observable behavior remains the same.
5. Treat guards as the safety net. Do not weaken behavior checks, but do not be timid because checks exist. If a cleanup is plausible, behavior-preserving, and aimed at measured complexity, do it.
6. Re-run `cstat --path . scorecard --top 20` when practical. Run targeted behavior checks when they are cheap and directly cover risky edits; the grind gate will run the configured oracle after you exit.
7. Update `.loop/notes.md`. The first line must be `STATUS: IN_PROGRESS` or `STATUS: DONE`. Include starting cost, ending cost if measured, files changed, complexity contributors attacked, guard failures repaired, and concrete next targets.
8. Exit. Do not loop manually; Yoke handles the next iteration.

## STATUS Signaling

Use `STATUS: IN_PROGRESS` when more cleanup or repair is needed.
Use `STATUS: DONE` only when current `code_complexity_cost` is lower than the baseline and you expect the behavior oracle plus optional benchmark to pass.

## What the Grind Gate Checks

The guard command runs `python3 .loop/grind-gate.py`. It fails unless:

1. the behavior oracle command succeeds;
2. the optional benchmark command succeeds, when configured;
3. current `cstat --path . --json scorecard` has lower `code_complexity_cost` than the baseline.

If the gate fails, read `.loop/guard-results.md` on the next iteration and repair exactly that failure before pushing more complexity reduction.

## Rules

- No git operations. Do not commit, push, branch, reset, stash, or modify git config.
- Do not modify `.loop/protocol.md`, `.loop/yoke.conf`, `.loop/grind-gate.py`, or `.loop/cstat-baseline.json`.
- Do not weaken, delete, skip, or rewrite behavior checks to make the gate pass.
- Do not edit behavior-defining tests, benches, benchmarks, examples, snapshots, or fixtures unless the user explicitly made behavior change part of the task. If the generated `yoke.conf` has no-modify rules commented out, infer the freeze rule from this protocol.
- Prefer deletion, simplification, and surface-area reduction over new abstractions.
- Push aggressively within the iteration; the default failure mode to avoid is timid under-cleanup.
