mod boundary;
mod config;
mod guard;
mod json;
mod signal;
mod stream;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};

use config::Config;

const CONF_PATH: &str = ".loop/guard.conf";
const NOTES_PATH: &str = ".loop/notes.md";
const GUARD_RESULTS_PATH: &str = ".loop/guard-results.md";
const PROTOCOL_PATH: &str = ".loop/protocol.md";
const PLAN_PATH: &str = ".loop/plan.md";
const TASK_PATH: &str = ".loop/task.md";
const JUDGE_PATH: &str = ".loop/judge.md";
const VERDICT_PATH: &str = ".loop/verdict.md";

const DEFAULT_PROTOCOL: &str = r#"# Protocol: Automated CI Loop

You are operating inside an automated loop — not a conversation. A bash script launched you, and will run guard checks after you exit. You do not interact with a human during this session.

## Files

| File | You can | Purpose |
|------|---------|---------|
| `.loop/protocol.md` | read | This document. Your instructions. |
| `.loop/plan.md` | read | The feature plan. Stages to implement. |
| `.loop/notes.md` | read + write | Your scratchpad. Persists across iterations. |
| `.loop/guard-results.md` | read | Guard results from the last iteration. |
| `.loop/guard.conf` | read | Loop configuration. Scope rules, guards, settings. |

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

## What the Guards Check

After you exit, the outer loop runs guards defined in `.loop/guard.conf`.

1. **Diff boundary check** — Always runs first. Verifies every file you changed
   or created is within the scope rules defined in `.loop/guard.conf`. The rules:
   - `allow PREFIX` — anything goes: add, modify, delete.
   - `add-only PREFIX` — may only add lines; no removing existing lines.
   - `no-modify PREFIX` — zero modifications allowed.
   - No matching rule — change is denied.
   - Most-specific (longest) prefix wins when rules overlap.
   If the boundary check fails, all subsequent guards are skipped.
2. **Configured guards** — Read the `guard` lines in `.loop/guard.conf` to see
   what commands run. Guards execute in order, fail-fast (first failure skips
   the rest).

You may run any commands you find useful during implementation.

## Rules

- **No git operations.** Do not commit, push, branch, or modify git config. The outer loop owns git.
- **Do not modify `protocol.md`, `plan.md`, or `guard.conf`.** These are read-only to you.
- **One stage per iteration.** Implement a single stage, update notes, and exit. Do not attempt multiple stages.
- **Retry discipline.** If you have failed on the same issue for 3 consecutive iterations (check your notes), try a fundamentally different approach. Do not repeat the same fix.
- **Be concise in notes.** Future-you needs signal, not noise. Record what matters: what stage, what changed, what broke, what to try next.
"#;

const DEFAULT_GUARD_CONF: &str = "\
# Guard configuration

claude      claude
image       claude-code-sandbox:latest

# Max lines of guard output to keep
max-tail    200

# Scope rules (diff boundary enforcement)
# Tags: allow (any change), add-only (new lines only), no-modify (no changes)
# Most-specific (longest) prefix wins.
allow       .

# Guards (run in order, fail-fast)
guard       cargo check
";

const DEFAULT_BRUTE_PROTOCOL: &str = r#"# Brute Protocol

You are a worker in an automated brute-force loop. Your single goal is described
in `.loop/task.md`. You iterate until a blind judge (a separate Claude instance
with zero implementation context) confirms the task is complete.

## Files

| File | Access | Purpose |
|---|---|---|
| `.loop/protocol.md` | read | These instructions. |
| `.loop/task.md` | read | The goal. |
| `.loop/judge.md` | read | What the judge will test. Study this. |
| `.loop/notes.md` | read+write | Your scratchpad across iterations. |
| `.loop/verdict.md` | read | The judge's last verdict. |
| `.loop/ci.conf` | read | Configuration. |

## Per-Iteration

1. Read `.loop/task.md` — understand the goal.
2. Read `.loop/notes.md` — recall what you have tried.
3. Read `.loop/verdict.md` — the judge's exact complaints from last iteration.
4. Make changes to accomplish the task.
5. Update `.loop/notes.md` with what you changed and why.
6. Exit — the harness runs guards then the judge.

## Rules

- No git operations.
- Do not modify protocol.md, task.md, judge.md, or ci.conf.
- Study judge.md — knowing the test helps you pass it.
- The judge's feedback is ground truth. Fix what they say is broken.
- After 3 similar verdicts, try a fundamentally different approach.
- Be concise in notes.
"#;

const DEFAULT_BRUTE_CONF: &str = "\
# Brute runner configuration

runner      brute
claude      claude
max-tail    200

allow       .

# Guards (run before judge, fail-fast)
guard       cargo check

# Judge — a fresh Claude that verifies the feature blind.
# The value is the Claude binary (same format as the claude directive).
judge       claude
";

/// Files that Claude must not be allowed to permanently alter.
const PROTECTED_FILES: &[&str] = &[PROTOCOL_PATH, PLAN_PATH, CONF_PATH];

// ANSI helpers
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";

fn log(msg: &str) {
    eprintln!("{}{}[yoke]{} {}", BOLD, CYAN, RESET, msg);
}

fn log_error(msg: &str) {
    eprintln!("{}{}[yoke] ERROR:{} {}", BOLD, RED, RESET, msg);
}

fn print_usage() {
    eprintln!(
        "{}{}yoke{} — LLM loop harness{}",
        BOLD, CYAN, RESET, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke <command> [options]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}COMMANDS:{}", BOLD, RESET);
    eprintln!(
        "  {}init{}                    Initialize .loop/ directory with default files",
        BOLD, RESET
    );
    eprintln!(
        "  {}init --runner <type>{}    Initialize for a specific runner (loop or brute)",
        BOLD, RESET
    );
    eprintln!(
        "  {}run{}                     Launch the CI loop (invoke Claude, run guards, iterate)",
        BOLD, RESET
    );
    eprintln!(
        "  {}run --dry-run{}           Single iteration: boundary check + guards only, no Claude",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}OPTIONS:{}", BOLD, RESET);
    eprintln!("  --help, -h       Show this help message");
    eprintln!();
    eprintln!(
        "{}CONFIG:{}  {}{}",
        BOLD, RESET, DIM, CONF_PATH
    );
    eprintln!(
        "{}EXIT:{}   0=success  1=guard failure  2=usage error",
        BOLD, RESET
    );
}

fn print_run_help() {
    eprintln!(
        "{}{}yoke run{} — execute the CI loop{}",
        BOLD, CYAN, RESET, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke run [--dry-run]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}OPTIONS:{}", BOLD, RESET);
    eprintln!("  --dry-run    Run one iteration without invoking Claude");
    eprintln!("               (boundary check + configured guards only)");
    eprintln!();
    eprintln!("{}RUNNER TYPES:{}", BOLD, RESET);
    eprintln!("  The runner type is determined by the 'runner' directive in {}.", CONF_PATH);
    eprintln!("  {}loop{}    (default) Staged plan runner — iterates until STATUS: DONE and guards pass.", BOLD, RESET);
    eprintln!("  {}brute{}   Blind-judge runner — iterates until an independent judge says PASS.", BOLD, RESET);
    eprintln!();
    eprintln!("{}WORKFLOW (loop):{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Backup protected files (protocol.md, plan.md, guard.conf)");
    eprintln!("  3. Per iteration:");
    eprintln!("     a. Restore protected files");
    eprintln!("     b. Invoke Claude with stream-json output");
    eprintln!("     c. Run diff boundary check");
    eprintln!("     d. Run configured guard commands");
    eprintln!("  4. Exit when STATUS: DONE and all guards pass");
    eprintln!();
    eprintln!("{}WORKFLOW (brute):{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Backup protected files (protocol.md, task.md, judge.md, ci.conf)");
    eprintln!("  3. Per iteration:");
    eprintln!("     a. Restore protected files, clear verdict");
    eprintln!("     b. Invoke worker Claude");
    eprintln!("     c. Run diff boundary check + configured guards");
    eprintln!("     d. If guards pass, invoke judge (fresh Claude, zero context)");
    eprintln!("  4. Exit when judge returns VERDICT: PASS");
}

fn print_init_help() {
    eprintln!(
        "{}{}yoke init{} — initialize .loop/ directory{}",
        BOLD, CYAN, RESET, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke init [--runner <type>]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}OPTIONS:{}", BOLD, RESET);
    eprintln!("  --runner <type>  Runner type: loop (default) or brute");
    eprintln!();
    eprintln!("{}RUNNER TYPES:{}", BOLD, RESET);
    eprintln!("  {}loop{}   (default) Staged plan runner. Creates:", BOLD, RESET);
    eprintln!("           ci.conf, protocol.md, plan.md, notes.md, guard-results.md");
    eprintln!();
    eprintln!("  {}brute{}  Blind-judge runner. Creates:", BOLD, RESET);
    eprintln!("           ci.conf, protocol.md, task.md, judge.md, notes.md, verdict.md");
    eprintln!();
    eprintln!("Existing files are never overwritten.");
}

/// Holds loop state and cleans up on drop.
///
/// Drop kills any running Claude child process and removes the backup
/// directory. Note: Drop does not run on raw SIGINT — the child process
/// shares the process group and receives the signal directly, and the
/// backup temp dir is small and inconsequential.
struct LoopRunner {
    backup_dir: PathBuf,
    child: Option<Child>,
}

impl LoopRunner {
    fn new(backup_dir: PathBuf) -> Self {
        Self {
            backup_dir,
            child: None,
        }
    }
}

impl Drop for LoopRunner {
    fn drop(&mut self) {
        // Kill any running child process
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        signal::set_child_pid(0);
        // Remove backup directory
        let _ = fs::remove_dir_all(&self.backup_dir);
    }
}

/// Check that required loop files exist, create notes if missing.
fn preflight() {
    for path in &[PROTOCOL_PATH, PLAN_PATH, CONF_PATH] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }
    // Ensure notes file exists
    if !Path::new(NOTES_PATH).exists() {
        if let Err(e) = fs::write(NOTES_PATH, "") {
            log_error(&format!("cannot create {}: {}", NOTES_PATH, e));
            process::exit(1);
        }
    }
    // Clear guard results
    let _ = fs::write(GUARD_RESULTS_PATH, "");
}

/// Copy protected files to a temp directory. Returns the backup path.
fn backup_protected() -> PathBuf {
    let backup_dir = std::env::temp_dir().join(format!("yoke-backup-{}", process::id()));
    if let Err(e) = fs::create_dir_all(&backup_dir) {
        log_error(&format!("failed to create backup dir: {}", e));
        process::exit(1);
    }
    for path in PROTECTED_FILES {
        let src = Path::new(path);
        if src.exists() {
            let dest = backup_dir.join(src.file_name().unwrap());
            if let Err(e) = fs::copy(src, &dest) {
                log_error(&format!("failed to backup {}: {}", path, e));
                process::exit(1);
            }
        }
    }
    backup_dir
}

/// Restore protected files from backup.
fn restore_protected(backup_dir: &Path) {
    for path in PROTECTED_FILES {
        let src_name = Path::new(path).file_name().unwrap();
        let backup_file = backup_dir.join(src_name);
        if backup_file.exists() {
            if let Err(e) = fs::copy(&backup_file, path) {
                eprintln!(
                    "{}{}[yoke] WARNING:{} failed to restore {}: {}",
                    BOLD, YELLOW, RESET, path, e
                );
            }
        }
    }
}

/// Check the first line of notes.md for STATUS: DONE.
fn is_done() -> bool {
    match fs::read_to_string(NOTES_PATH) {
        Ok(content) => content.starts_with("STATUS: DONE"),
        Err(_) => false,
    }
}

/// Invoke Claude, piping stdout through the stream filter.
/// The Child is stored in `runner` for cleanup-on-drop safety.
/// Returns the child's exit status success.
fn invoke_claude(runner: &mut LoopRunner, config: &Config, iteration: u32) -> bool {
    let claude_args = [
        "--verbose",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--dangerously-skip-permissions",
        "-p",
        "Read .loop/protocol.md and follow its instructions.",
    ];

    let mut cmd = if let Some(ref image) = config.image {
        log(&format!(
            "Launching Claude in container (iteration {})...",
            iteration
        ));

        let workdir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string();

        let mut c = Command::new("docker");
        c.args([
            "run",
            "--rm",
            "-i",
            "--network=host",
            "--cap-add=NET_ADMIN",
            "--cap-add=NET_RAW",
        ]);

        // Bind-mount the workspace
        c.arg("-v").arg(format!("{}:/workspace", workdir));
        c.arg("-w").arg("/workspace");

        // Forward all CLAUDE_* and ANTHROPIC_* env vars
        for (key, val) in std::env::vars() {
            if key.starts_with("CLAUDE_") || key.starts_with("ANTHROPIC_") {
                c.arg("-e").arg(format!("{}={}", key, val));
            }
        }

        // Mount host claude config/auth directory (read-write — Claude needs to write state)
        if let Some(home) = std::env::var_os("HOME") {
            let home_path = PathBuf::from(&home);
            let claude_dir = home_path.join(".claude");
            if claude_dir.exists() {
                c.arg("-v").arg(format!(
                    "{}:/home/node/.claude",
                    claude_dir.display()
                ));
            }
            // Mount .claude.json for onboarding/theme/user config
            let claude_json = home_path.join(".claude.json");
            if claude_json.exists() {
                c.arg("-v").arg(format!(
                    "{}:/home/node/.claude.json",
                    claude_json.display()
                ));
            }
        }

        c.arg(image.as_str());
        c.arg(&config.claude_bin);
        c.args(claude_args);
        c
    } else {
        log(&format!("Launching Claude (iteration {})...", iteration));
        let mut c = Command::new(&config.claude_bin);
        c.args(claude_args);
        c
    };

    let mut child = match cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let bin = if config.image.is_some() { "docker" } else { &config.claude_bin };
            log_error(&format!("failed to spawn '{}': {}", bin, e));
            return false;
        }
    };

    // Register child PID so signal handler can kill it to unblock pipe reads
    signal::set_child_pid(child.id() as i32);

    // Compute log path
    let log_path = config.log_dir.as_ref().map(|dir| {
        std::path::PathBuf::from(dir).join(format!("iteration-{}.jsonl", iteration))
    });

    // Take stdout and feed through stream filter
    if let Some(stdout) = child.stdout.take() {
        // Store child in runner before blocking on stream filter,
        // so Drop can kill it if we're interrupted.
        runner.child = Some(child);
        stream::filter_stream(stdout, log_path.as_deref());
        if signal::interrupted() {
            return false;
        }
    } else {
        runner.child = Some(child);
    }

    // Wait for the child to exit
    let status = if let Some(ref mut child) = runner.child {
        match child.wait() {
            Ok(s) => {
                if signal::interrupted() {
                    return false;
                }
                s.success()
            }
            Err(e) => {
                log_error(&format!("failed to wait on claude process: {}", e));
                false
            }
        }
    } else {
        false
    };

    // Child has exited; clear it from runner
    signal::set_child_pid(0);
    runner.child = None;

    log(&format!(
        "Claude exited ({})",
        if status { "success" } else { "failure" }
    ));
    status
}

/// Invoke the judge — a fresh Claude with zero implementation context.
/// Returns true if the judge's verdict is PASS.
fn invoke_judge(runner: &mut LoopRunner, config: &Config, iteration: u32) -> bool {
    let judge_bin = match &config.judge {
        Some(bin) => bin.clone(),
        None => {
            log_error("no 'judge' directive in config — cannot invoke judge");
            return false;
        }
    };

    let judge_prompt = "\
Read .loop/judge.md. It describes what to verify.
Test the feature exactly as described. You have full shell access.
When done, write your verdict to .loop/verdict.md in this exact format:

VERDICT: PASS

<what you tested and what happened>

or

VERDICT: FAIL

<what you tested, what went wrong, and what the correct behavior should be>";

    let claude_args = [
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "-p",
        judge_prompt,
    ];

    let mut cmd = if let Some(ref image) = config.image {
        log(&format!(
            "Launching judge in container (iteration {})...",
            iteration
        ));

        let workdir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string();

        let mut c = Command::new("docker");
        c.args([
            "run",
            "--rm",
            "-i",
            "--network=host",
            "--cap-add=NET_ADMIN",
            "--cap-add=NET_RAW",
        ]);
        c.arg("-v").arg(format!("{}:/workspace", workdir));
        c.arg("-w").arg("/workspace");

        for (key, val) in std::env::vars() {
            if key.starts_with("CLAUDE_") || key.starts_with("ANTHROPIC_") {
                c.arg("-e").arg(format!("{}={}", key, val));
            }
        }

        if let Some(home) = std::env::var_os("HOME") {
            let home_path = PathBuf::from(&home);
            let claude_dir = home_path.join(".claude");
            if claude_dir.exists() {
                c.arg("-v").arg(format!(
                    "{}:/home/node/.claude",
                    claude_dir.display()
                ));
            }
            let claude_json = home_path.join(".claude.json");
            if claude_json.exists() {
                c.arg("-v").arg(format!(
                    "{}:/home/node/.claude.json",
                    claude_json.display()
                ));
            }
        }

        c.arg(image.as_str());
        c.arg(&judge_bin);
        c.args(claude_args);
        c
    } else {
        log(&format!("Launching judge (iteration {})...", iteration));
        let mut c = Command::new(&judge_bin);
        c.args(claude_args);
        c
    };

    let mut child = match cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let bin = if config.image.is_some() { "docker" } else { &judge_bin };
            log_error(&format!("failed to spawn judge '{}': {}", bin, e));
            return false;
        }
    };

    signal::set_child_pid(child.id() as i32);

    let log_path = config.log_dir.as_ref().map(|dir| {
        std::path::PathBuf::from(dir).join(format!("judge-{}.jsonl", iteration))
    });

    if let Some(stdout) = child.stdout.take() {
        runner.child = Some(child);
        stream::filter_stream(stdout, log_path.as_deref());
        if signal::interrupted() {
            return false;
        }
    } else {
        runner.child = Some(child);
    }

    let status = if let Some(ref mut child) = runner.child {
        match child.wait() {
            Ok(s) => {
                if signal::interrupted() {
                    return false;
                }
                s.success()
            }
            Err(e) => {
                log_error(&format!("failed to wait on judge process: {}", e));
                false
            }
        }
    } else {
        false
    };

    signal::set_child_pid(0);
    runner.child = None;

    if !status {
        log_error("Judge process exited with failure");
        return false;
    }

    // Read verdict
    match fs::read_to_string(VERDICT_PATH) {
        Ok(content) => {
            let pass = content.starts_with("VERDICT: PASS");
            if pass {
                log(&format!("{}{}Judge verdict: PASS{}", GREEN, BOLD, RESET));
            } else {
                log(&format!("{}Judge verdict: FAIL{}", YELLOW, RESET));
            }
            pass
        }
        Err(e) => {
            log_error(&format!("failed to read {}: {}", VERDICT_PATH, e));
            false
        }
    }
}

/// Run boundary check and all configured guards.
/// Returns true if everything passed.
fn run_all_guards(config: &Config) -> bool {
    // Boundary check first
    log("Running diff boundary check...");
    let boundary = boundary::check(config);

    if boundary.passed {
        log(&format!("{}  Boundary check: PASS{}", GREEN, RESET));
    } else {
        log(&format!(
            "{}  Boundary check: FAIL ({} violation(s)){}",
            RED,
            boundary.violations.len(),
            RESET
        ));
        for v in &boundary.violations {
            eprintln!("  {}", v);
        }

        // Write boundary failure to guard results, mark configured guards as skipped
        let mut md = String::new();
        md.push_str("## diff boundary check \u{2014} FAIL\n\n```\n");
        for v in &boundary.violations {
            md.push_str(v);
            md.push('\n');
        }
        md.push_str("```\n\n");
        for g in &config.guards {
            md.push_str(&format!("## {} \u{2014} SKIPPED\n\n", g));
            md.push_str("```\nSkipped due to diff boundary violation.\n```\n\n");
        }
        let _ = fs::write(GUARD_RESULTS_PATH, &md);
        return false;
    }

    // Boundary passed — write that to results then run configured guards
    let results_path = Path::new(GUARD_RESULTS_PATH);

    let boundary_md = "## diff boundary check \u{2014} PASS\n\n```\nAll changed files are within allowed boundaries.\n```\n\n";

    log("Running configured guards...");
    let guard_results = guard::run_guards(&config.guards, config.max_tail, results_path);

    // Prepend boundary result to whatever guard::run_guards wrote
    let existing = fs::read_to_string(results_path).unwrap_or_default();
    let combined = format!("{}{}", boundary_md, existing);
    let _ = fs::write(results_path, combined);

    let all_passed = guard_results.iter().all(|r| r.passed);
    for r in &guard_results {
        let status = if r.skipped {
            format!("{}SKIPPED{}", YELLOW, RESET)
        } else if r.passed {
            format!("{}PASS{}", GREEN, RESET)
        } else {
            format!("{}FAIL{}", RED, RESET)
        };
        log(&format!("  {}: {}", r.name, status));
    }

    all_passed
}

fn init(runner: &str) -> i32 {
    let loop_dir = Path::new(".loop");
    if !loop_dir.exists() {
        if let Err(e) = fs::create_dir(loop_dir) {
            log_error(&format!("failed to create .loop/: {}", e));
            return 1;
        }
        log("Created .loop/");
    }

    let files: Vec<(&str, &str)> = match runner {
        "loop" => vec![
            (CONF_PATH, DEFAULT_GUARD_CONF),
            (PROTOCOL_PATH, DEFAULT_PROTOCOL),
            (PLAN_PATH, ""),
            (NOTES_PATH, ""),
            (GUARD_RESULTS_PATH, ""),
        ],
        "brute" => vec![
            (CONF_PATH, DEFAULT_BRUTE_CONF),
            (PROTOCOL_PATH, DEFAULT_BRUTE_PROTOCOL),
            (TASK_PATH, ""),
            (JUDGE_PATH, ""),
            (NOTES_PATH, ""),
            (VERDICT_PATH, ""),
        ],
        other => {
            log_error(&format!("unknown runner type '{}'", other));
            return 2;
        }
    };

    log(&format!("Initializing '{}' runner...", runner));

    for (path, content) in &files {
        let p = Path::new(path);
        if p.exists() {
            log(&format!("skip (exists): {}", path));
        } else {
            if let Err(e) = fs::write(p, content) {
                log_error(&format!("failed to write {}: {}", path, e));
                return 1;
            }
            log(&format!("created: {}", path));
        }
    }

    0
}

fn run_loop(dry_run: bool) -> i32 {
    // Load config
    let config = match Config::load(Path::new(CONF_PATH)) {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    };

    log(&format!(
        "Config loaded: claude={}, max_tail={}, {} scope rules, {} guards{}",
        config.claude_bin,
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len(),
        config.image.as_ref().map_or(
            String::from(", sandbox=off"),
            |img| format!(", image={}", img)
        )
    ));

    // Preflight
    preflight();
    log("Preflight OK");

    // Backup protected files and create the runner (Drop handles cleanup)
    let backup_dir = backup_protected();
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut iteration: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }
        iteration += 1;
        eprintln!();
        eprintln!(
            "{}{}╔══════════════════════════════════════╗{}",
            BOLD, CYAN, RESET
        );
        eprintln!(
            "{}{}║         Iteration {:>4}              ║{}",
            BOLD, CYAN, iteration, RESET
        );
        eprintln!(
            "{}{}╚══════════════════════════════════════╝{}",
            BOLD, CYAN, RESET
        );

        // Restore protected files
        restore_protected(&runner.backup_dir);

        // Clear guard results
        let _ = fs::write(GUARD_RESULTS_PATH, "");

        if dry_run {
            log("(dry-run) Skipping Claude invocation");
        } else if !invoke_claude(&mut runner, &config, iteration) {
            log_error("Claude invocation failed — aborting loop");
            return 1;
        }

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }

        // Run guards
        let guards_passed = run_all_guards(&config);

        if guards_passed {
            log(&format!(
                "{}{}All guards passed{}",
                GREEN, BOLD, RESET
            ));

            if dry_run {
                log("(dry-run) Guards passed \u{2014} exiting after one iteration");
                return 0;
            }

            if is_done() {
                eprintln!();
                eprintln!(
                    "{}{}  STATUS: DONE and all guards pass \u{2014} loop complete  {}",
                    GREEN, BOLD, RESET
                );
                eprintln!();
                return 0;
            } else {
                log("Guards passed but STATUS is not DONE \u{2014} continuing");
            }
        } else {
            log(&format!(
                "{}Some guards failed \u{2014} Claude will see results next iteration{}",
                YELLOW, RESET
            ));

            if dry_run {
                log("(dry-run) Exiting after one iteration");
                return 1;
            }
        }
    }
}

/// Protected files for the brute runner.
const BRUTE_PROTECTED_FILES: &[&str] = &[PROTOCOL_PATH, TASK_PATH, JUDGE_PATH, CONF_PATH];

fn run_brute(dry_run: bool) -> i32 {
    // Load config
    let config = match Config::load(Path::new(CONF_PATH)) {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    };

    log(&format!(
        "Config loaded: runner=brute, claude={}, judge={}, max_tail={}, {} scope rules, {} guards{}",
        config.claude_bin,
        config.judge.as_deref().unwrap_or("(none)"),
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len(),
        config.image.as_ref().map_or(
            String::from(", sandbox=off"),
            |img| format!(", image={}", img)
        )
    ));

    // Preflight: require protocol.md, task.md, judge.md, ci.conf
    for path in &[PROTOCOL_PATH, TASK_PATH, JUDGE_PATH, CONF_PATH] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }

    // Create notes.md and verdict.md if missing
    for path in &[NOTES_PATH, VERDICT_PATH] {
        if !Path::new(path).exists() {
            if let Err(e) = fs::write(path, "") {
                log_error(&format!("cannot create {}: {}", path, e));
                process::exit(1);
            }
        }
    }

    // Clear verdict
    let _ = fs::write(VERDICT_PATH, "");

    log("Preflight OK");

    // Backup protected files
    let backup_dir = std::env::temp_dir().join(format!("yoke-backup-{}", process::id()));
    if let Err(e) = fs::create_dir_all(&backup_dir) {
        log_error(&format!("failed to create backup dir: {}", e));
        process::exit(1);
    }
    for path in BRUTE_PROTECTED_FILES {
        let src = Path::new(path);
        if src.exists() {
            let dest = backup_dir.join(src.file_name().unwrap());
            if let Err(e) = fs::copy(src, &dest) {
                log_error(&format!("failed to backup {}: {}", path, e));
                process::exit(1);
            }
        }
    }
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut iteration: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }
        iteration += 1;
        eprintln!();
        eprintln!(
            "{}{}╔══════════════════════════════════════╗{}",
            BOLD, CYAN, RESET
        );
        eprintln!(
            "{}{}║     Brute Iteration {:>4}             ║{}",
            BOLD, CYAN, iteration, RESET
        );
        eprintln!(
            "{}{}╚══════════════════════════════════════╝{}",
            BOLD, CYAN, RESET
        );

        // Restore protected files
        for path in BRUTE_PROTECTED_FILES {
            let src_name = Path::new(path).file_name().unwrap();
            let backup_file = runner.backup_dir.join(src_name);
            if backup_file.exists() {
                if let Err(e) = fs::copy(&backup_file, path) {
                    eprintln!(
                        "{}{}[yoke] WARNING:{} failed to restore {}: {}",
                        BOLD, YELLOW, RESET, path, e
                    );
                }
            }
        }

        // Clear verdict
        let _ = fs::write(VERDICT_PATH, "");

        if dry_run {
            log("(dry-run) Skipping Claude invocation");
        } else if !invoke_claude(&mut runner, &config, iteration) {
            log_error("Claude invocation failed — aborting loop");
            return 1;
        }

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }

        // Run guards
        let guards_passed = run_all_guards(&config);

        if !guards_passed {
            log(&format!(
                "{}Guards failed \u{2014} worker will see results next iteration{}",
                YELLOW, RESET
            ));
            if dry_run {
                log("(dry-run) Exiting after one iteration");
                return 1;
            }
            continue;
        }

        log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));

        if dry_run {
            log("(dry-run) Guards passed \u{2014} exiting after one iteration");
            return 0;
        }

        // Guards passed — invoke judge
        log("Guards passed \u{2014} invoking judge...");
        let pass = invoke_judge(&mut runner, &config, iteration);

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }

        if pass {
            eprintln!();
            eprintln!(
                "{}{}  Judge says PASS \u{2014} brute loop complete  {}",
                GREEN, BOLD, RESET
            );
            eprintln!();
            return 0;
        }

        log(&format!(
            "{}Judge says FAIL \u{2014} worker will see verdict next iteration{}",
            YELLOW, RESET
        ));
    }
}

fn main() {
    signal::install();
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    match args[1].as_str() {
        "init" => {
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_init_help();
                return;
            }
            let mut runner = "loop";
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--runner" => {
                        i += 1;
                        if i >= args.len() {
                            log_error("--runner requires a value (loop or brute)");
                            process::exit(2);
                        }
                        runner = &args[i];
                    }
                    other => {
                        log_error(&format!("unknown flag '{}'", other));
                        print_init_help();
                        process::exit(2);
                    }
                }
                i += 1;
            }
            process::exit(init(runner));
        }
        "run" => {
            // Check for --help before other flags
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_run_help();
                return;
            }
            let dry_run = args.get(2).is_some_and(|a| a == "--dry-run");
            if args.len() > 2 && !dry_run {
                log_error(&format!("unknown flag '{}'", args[2]));
                print_run_help();
                process::exit(2);
            }
            // Load config to determine runner type
            let config = match Config::load(Path::new(CONF_PATH)) {
                Ok(c) => c,
                Err(e) => {
                    log_error(&e);
                    process::exit(1);
                }
            };
            match config.runner.as_str() {
                "loop" => process::exit(run_loop(dry_run)),
                "brute" => process::exit(run_brute(dry_run)),
                other => {
                    log_error(&format!("unknown runner type '{}' in config", other));
                    process::exit(2);
                }
            }
        }
        "--help" | "-h" | "help" => {
            print_usage();
        }
        other => {
            log_error(&format!("unknown command '{}'", other));
            print_usage();
            process::exit(2);
        }
    }
}
