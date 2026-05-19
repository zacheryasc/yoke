# Context trim — design notes

## What's shipped

Worker sessions are now resumed across iterations with `claude --resume <sid>`,
and the session JSONL is surgically trimmed between rounds so Anthropic's
prefix cache stays warm without paying for irrelevant history.

- Agent declares a `KEEP: <paths>` line in `.loop/notes.md`.
- Yoke locates `~/.claude/projects/<cwd-slug>/<sid>.jsonl` and rewrites it
  to retain only: bootstrap records, the initial user prompt, `Read`
  tool_use/tool_result pairs for kept paths, and attachments. Drops:
  `thinking`, intermediate `text`, every non-`Read` tool_use, `Read`s of
  non-kept files, and the matching `tool_result`s. Re-links the
  parent-uuid chain across the gaps; validates tool_use ↔ tool_result
  pairing before commit; keeps a `.bak`.
- Judge (brute) and scoper (saga) always run fresh — independence per
  behavioral-specification §2.1.
- `YOKE_DISABLE_SESSION_TRIM=1` is the escape hatch.

## What still needs adding

1. **Format-drift guard.** The Claude Code session JSONL is undocumented.
   A future CLI release could rename a field, change content-block shape,
   or move the file. The validator catches most damage post-trim, but
   pre-trim we should fingerprint the format (e.g., known top-level keys
   on bootstrap records) and bail if it drifts. Today we trust + bail on
   validate-fail; a positive check would be safer.

2. **Recovery from `--resume` failure.** If Claude rejects the resumed
   session (deleted, corrupted, version skew), the iteration aborts. We
   should detect this from the spawn's exit/early stream error and
   transparently retry once with no `--resume` (treat last_session_id as
   stale).

3. **Sandboxed runs.** When `image` is set, the agent runs inside a
   Docker container — the session JSONL lives in the container's home,
   not the host's. Today `trim_worker_session` no-ops in that case
   (silent). Either mount the session dir into the container, or run the
   trim inside the container, or document the limitation.

4. **OpenCode backend.** Trim is Claude-specific. OpenCode users get
   `--resume` benefits skipped (different session model). If OpenCode
   becomes a first-class target, we need an analogous trim or a
   reasoned-down equivalent.

5. **Fork policy.** v1 has no forking: the trim alone bounds growth.
   But sessions still grow monotonically in the *kept* portion, and
   long-running tasks will eventually want a hard reset. A
   `context-fork-every N` directive (or agent-declared `RESET: TRUE`)
   would let users break the conversation cleanly at stage boundaries.

6. **Periodic-agent isolation.** Periodics spawn fresh sessions but
   share the project's session directory. If a periodic ever needed its
   own resumable continuity (e.g., a reviewer agent that learns over
   runs), today there's no separate session-id tracking for it.

7. **Saga handoff.** Across saga cycles the scoper is fresh and reads
   `saga-log.md` to reconstruct context. A future variant could let the
   *brute worker inside saga* keep its session across cycles when the
   scoper produces a closely-related sub-plan — but only if the scoper
   signals it (otherwise context bleeds between unrelated chunks).

Next steps involve creating some test vectors to help make behavior standardized
