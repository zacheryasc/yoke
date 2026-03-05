#![warn(clippy::string_slice)]

mod boundary;
mod config;
mod guard;
mod heartbeat;
mod json;
mod registry;
mod signal;
mod stream;
mod stream_opencode;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};

use config::{Backend, Config};

const CONF_PATH: &str = ".loop/yoke.conf";
const NOTES_PATH: &str = ".loop/notes.md";
const GUARD_RESULTS_PATH: &str = ".loop/guard-results.md";
const PROTOCOL_PATH: &str = ".loop/protocol.md";
const PLAN_PATH: &str = ".loop/plan.md";
const JUDGE_PATH: &str = ".loop/judge.md";
const VERDICT_PATH: &str = ".loop/verdict.md";

const DEFAULT_PROTOCOL: &str = include_str!("templates/loop/protocol.md");
const DEFAULT_YOKE_CONF: &str = include_str!("templates/loop/yoke.conf");
const DEFAULT_BRUTE_PROTOCOL: &str = include_str!("templates/brute/protocol.md");
const DEFAULT_BRUTE_CONF: &str = include_str!("templates/brute/yoke.conf");
const DEFAULT_JUDGE: &str = include_str!("templates/brute/judge.md");
const DEFAULT_BRIEFING: &str = include_str!("templates/loop/briefing.md");
const DEFAULT_SAGA_PROTOCOL: &str = include_str!("templates/saga/saga-protocol.md");
const DEFAULT_SAGA_WORKER_PROTOCOL: &str = include_str!("templates/saga/protocol.md");
const DEFAULT_SAGA_JUDGE: &str = include_str!("templates/saga/judge.md");
const DEFAULT_SAGA_CONF: &str = include_str!("templates/saga/yoke.conf");

const LAYER_REPL: &str = include_str!("templates/layers/repl.md");

const BRIEFING_PATH: &str = ".loop/briefing.md";
const SAGA_PROTOCOL_PATH: &str = ".loop/saga-protocol.md";
const SPECIFICATION_PATH: &str = ".loop/specification.md";
const SAGA_NOTES_PATH: &str = ".loop/saga-notes.md";
const DECISIONS_PATH: &str = ".loop/decisions.md";
const SUB_PLAN_PATH: &str = ".loop/sub-plan.md";
const STASH_DIR: &str = ".loop/.stash";

/// Parse the plan file and notes.md to determine stage progress.
/// Returns `Some((completed, total))` if the plan has parseable `## Stage` headers.
fn stage_progress(plan_path: &str) -> Option<(usize, usize)> {
    let plan = fs::read_to_string(plan_path).ok()?;
    let total = plan.lines().filter(|l| l.starts_with("## Stage")).count();
    if total == 0 {
        return None;
    }

    let notes = fs::read_to_string(NOTES_PATH).unwrap_or_default();
    // Count lines matching "## Stage N — DONE" pattern in notes
    let completed = notes
        .lines()
        .filter(|l| {
            l.starts_with("## Stage")
                && (l.contains("DONE") || l.contains("done") || l.contains("Done"))
        })
        .count();

    Some((completed, total))
}

/// Render a compact progress bar like `▐████░░░░▌ 3/7 stages`.
fn format_progress_bar(completed: usize, total: usize) -> String {
    let bar_width = 10;
    let filled = if total > 0 {
        ((completed * bar_width + total / 2) / total).min(bar_width)
    } else {
        0
    };
    let empty = bar_width - filled;
    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!(
        "{}{}  ▐{}▌ {}/{} stages{}",
        DIM, GREEN, bar, completed, total, RESET
    )
}

// ANSI helpers
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[38;5;46m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[38;5;196m";
const BLUE: &str = "\x1b[38;5;75m";

fn log(msg: &str) {
    eprintln!("{}{}[yoke]{} {}", ORANGE, BOLD, RESET, msg);
}

fn log_error(msg: &str) {
    eprintln!("{}{}[yoke]{} {}{}ERROR:{} {}", ORANGE, BOLD, RESET, BOLD, RED, RESET, msg);
}

fn get_layer(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "repl" => Some(("REPL probing", LAYER_REPL)),
        _ => None,
    }
}

fn list_layers() -> &'static [&'static str] {
    &["repl"]
}

#[allow(clippy::string_slice)] // slices after ASCII prefixes ("## ", ". ")
fn apply_layers(names_arg: &str) -> i32 {
    let judge_path = Path::new(JUDGE_PATH);
    if !judge_path.exists() {
        log_error("no .loop/judge.md found — run 'yoke init brute' or 'yoke init saga' first");
        return 1;
    }

    let mut content = match fs::read_to_string(judge_path) {
        Ok(c) => c,
        Err(e) => {
            log_error(&format!("failed to read judge.md: {}", e));
            return 1;
        }
    };

    let names: Vec<&str> = names_arg.split(',').map(|s| s.trim()).collect();

    for name in &names {
        let (heading, layer_content) = match get_layer(name) {
            Some(v) => v,
            None => {
                log_error(&format!("unknown layer '{}' — use 'yoke layer --list'", name));
                return 2;
            }
        };

        if content.contains(heading) {
            log(&format!("skip (already present): {}", name));
            continue;
        }

        match content.find("\n## Verdict") {
            Some(pos) => {
                content.insert_str(pos, &format!("\n{}\n", layer_content.trim()));
            }
            None => {
                log_error("judge.md has no '## Verdict' section — cannot insert layer");
                return 1;
            }
        }
        log(&format!("applied layer: {}", name));
    }

    // Re-number ## N. sections sequentially
    let mut section_num = 1u32;
    let mut result = String::with_capacity(content.len());
    for line in content.lines() {
        if line.starts_with("## ") && line.len() > 4 {
            let rest = &line[3..];
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
                if after_digits.starts_with(". ") {
                    result.push_str(&format!("## {}. {}", section_num, &after_digits[2..]));
                    result.push('\n');
                    section_num += 1;
                    continue;
                }
            }
        }
        result.push_str(line);
        result.push('\n');
    }

    if let Err(e) = fs::write(judge_path, result.trim_end().to_string() + "\n") {
        log_error(&format!("failed to write judge.md: {}", e));
        return 1;
    }

    0
}

fn print_layer_help() {
    eprintln!("Usage: yoke layer <name>[,<name>,...]\n");
    eprintln!("  Append judge layers to .loop/judge.md\n");
    eprintln!("  --list   Show available layers");
    eprintln!("\nAvailable layers:");
    for name in list_layers() {
        eprintln!("  {}", name);
    }
}

fn print_usage() {
    eprintln!(
        "{}{}[yoke]{} LLM loop harness",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke <command> [options]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}COMMANDS:{}", BOLD, RESET);
    eprintln!(
        "  {}init{}             Initialize .loop/ for staged plan loop (default)",
        BOLD, RESET
    );
    eprintln!(
        "  {}init brute{}       Initialize .loop/ for brute mode (plan stages + judge)",
        BOLD, RESET
    );
    eprintln!(
        "  {}init saga{}        Initialize .loop/ for saga mode (scoper + brute loop)",
        BOLD, RESET
    );
    eprintln!(
        "  {}clean{}            Reset .loop/ working files to blank slate",
        BOLD, RESET
    );
    eprintln!(
        "  {}stash{}            Snapshot .loop/ state (stash log, stash checkout <hash>)",
        BOLD, RESET
    );
    eprintln!(
        "  {}layer{}            Append judge layers to .loop/judge.md",
        BOLD, RESET
    );
    eprintln!(
        "  {}run{}              Launch the loop (invoke Claude, run guards, iterate)",
        BOLD, RESET
    );
    eprintln!(
        "  {}run --dry-run{}    Single iteration: boundary check + guards only, no Claude",
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
        "{}{}[yoke run]{} execute the loop",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke run [--dry-run]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}OPTIONS:{}", BOLD, RESET);
    eprintln!("  --dry-run       Run one iteration without invoking Claude");
    eprintln!("                  (boundary check + configured guards only)");
    eprintln!("  --no-sandbox    Allow running without a container image");
    eprintln!();
    eprintln!("{}MODES:{}", BOLD, RESET);
    eprintln!("  Detected automatically from .loop/ contents.");
    eprintln!("  {}saga{}    Scoper + brute loop — detected when {} exists.", BOLD, RESET, SPECIFICATION_PATH);
    eprintln!("  {}brute{}   Plan stages + judge — detected when {} exists.", BOLD, RESET, JUDGE_PATH);
    eprintln!("  {}loop{}    (default) Staged plan loop — iterates until STATUS: DONE and guards pass.", BOLD, RESET);
    eprintln!();
    eprintln!("{}WORKFLOW (saga):{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Per saga cycle:");
    eprintln!("     a. Invoke scoper (Agent 1) — reads spec, writes sub-plan.md");
    eprintln!("     b. Run brute loop on sub-plan.md (Agent 2 + Agent 3)");
    eprintln!("     c. On pass: loop back to scoper for next chunk");
    eprintln!("     d. On bailout (3 judge fails): loop back to scoper to re-scope");
    eprintln!("  3. Exit when scoper signals STATUS: DONE");
    eprintln!();
    eprintln!("{}WORKFLOW (loop):{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Backup protected files (protocol.md, plan.md, yoke.conf)");
    eprintln!("  3. Per iteration:");
    eprintln!("     a. Restore protected files");
    eprintln!("     b. Invoke Claude with stream-json output");
    eprintln!("     c. Run diff boundary check");
    eprintln!("     d. Run configured guard commands");
    eprintln!("  4. Exit when STATUS: DONE and all guards pass");
    eprintln!();
    eprintln!("{}WORKFLOW (brute):{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Backup protected files (protocol.md, plan.md, task.md, judge.md, yoke.conf)");
    eprintln!("  3. Per brute attempt:");
    eprintln!("     a. Restore protected files, clear verdict");
    eprintln!("     b. Run plan loop (stages + guards until DONE)");
    eprintln!("     c. Invoke judge (fresh Claude, zero context)");
    eprintln!("     d. If judge FAILs, reset STATUS and retry");
    eprintln!("  4. Exit when judge returns VERDICT: PASS");
}

fn print_init_help() {
    eprintln!(
        "{}{}[yoke init]{} initialize .loop/ directory",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke init [brute|saga]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}MODES:{}", BOLD, RESET);
    eprintln!("  {}(default){}  Staged plan loop. Creates:", BOLD, RESET);
    eprintln!("              yoke.conf, protocol.md, briefing.md, plan.md, notes.md,");
    eprintln!("              guard-results.md");
    eprintln!();
    eprintln!("  {}brute{}      Plan stages + judge. Creates:", BOLD, RESET);
    eprintln!("              yoke.conf, protocol.md, briefing.md, plan.md, judge.md,");
    eprintln!("              notes.md, verdict.md, guard-results.md");
    eprintln!();
    eprintln!("  {}saga{}       Scoper + brute loop. Creates:", BOLD, RESET);
    eprintln!("              yoke.conf, saga-protocol.md, protocol.md, judge.md,");
    eprintln!("              specification.md, saga-notes.md, decisions.md,");
    eprintln!("              sub-plan.md, notes.md, verdict.md, guard-results.md");
    eprintln!();
    eprintln!("Existing files are never overwritten.");
    eprintln!();
    eprintln!("{}MODE SWITCHING:{}", BOLD, RESET);
    eprintln!("  Switching modes automatically snapshots the current mode's files");
    eprintln!("  into .loop/.modes/<mode>/ and restores any prior snapshot of the");
    eprintln!("  target mode. This lets you bounce between modes without losing work.");
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
///
/// When `clear_guard_results` is true (standalone loop), guard-results.md is
/// cleared on entry. When false (nested inside brute), the previous attempt's
/// guard feedback is preserved so the worker can read it.
fn preflight(plan_path: &str, clear_guard_results: bool) {
    for path in &[PROTOCOL_PATH, plan_path, CONF_PATH] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }
    // Ensure notes file exists
    if !Path::new(NOTES_PATH).exists()
        && let Err(e) = fs::write(NOTES_PATH, "")
    {
        log_error(&format!("cannot create {}: {}", NOTES_PATH, e));
        process::exit(1);
    }
    // Clear guard results only on standalone (non-nested) runs.
    // In brute mode, run_brute_inner() handles the initial clear and
    // retries preserve guard feedback for the worker.
    if clear_guard_results {
        let _ = fs::write(GUARD_RESULTS_PATH, "");
    }
}

/// Copy the given files to a backup directory inside .loop/. Returns the backup path.
fn backup_files(files: &[&str]) -> PathBuf {
    let backup_dir = PathBuf::from(format!(".loop/backups-{}", process::id()));
    if let Err(e) = fs::create_dir_all(&backup_dir) {
        log_error(&format!("failed to create backup dir: {}", e));
        process::exit(1);
    }
    for path in files {
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

/// Restore files from backup.
fn restore_files(backup_dir: &Path, files: &[&str]) {
    for path in files {
        let src_name = Path::new(path).file_name().unwrap();
        let backup_file = backup_dir.join(src_name);
        if backup_file.exists()
            && let Err(e) = fs::copy(&backup_file, path)
        {
            eprintln!(
                "{}{}[yoke]{} {}{}WARNING:{} failed to restore {}: {}",
                ORANGE, BOLD, RESET, BOLD, ORANGE, RESET, path, e
            );
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

/// Reset the STATUS line in notes.md back to IN_PROGRESS, preserving all other content.
fn reset_notes_status() {
    match fs::read_to_string(NOTES_PATH) {
        Ok(content) => {
            let new_content = if content.starts_with("STATUS: DONE") {
                content.replacen("STATUS: DONE", "STATUS: IN_PROGRESS", 1)
            } else if content.starts_with("STATUS: IN_PROGRESS") {
                content // already in progress
            } else {
                // No STATUS line at top — prepend one
                format!("STATUS: IN_PROGRESS\n{}", content)
            };
            if let Err(e) = fs::write(NOTES_PATH, new_content) {
                log_error(&format!("failed to reset STATUS in {}: {}", NOTES_PATH, e));
            } else {
                log("Reset STATUS to IN_PROGRESS for next brute attempt");
            }
        }
        Err(e) => {
            log_error(&format!("failed to read {}: {}", NOTES_PATH, e));
        }
    }
}

/// Build a Command for invoking the agent backend.
/// For Claude backend: uses `claude` CLI, optionally via docker.
/// For OpenCode backend: uses `opencode run --format json --model <model>`.
fn build_command(config: &Config, prompt: &str) -> Command {
    match config.backend() {
        Backend::Claude => {
            let claude_args = [
                "--verbose",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--dangerously-skip-permissions",
                "-p",
                prompt,
            ];
            if let Some(ref image) = config.image {
                let workdir = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .to_string_lossy()
                    .to_string();

                let mut c = Command::new("docker");
                c.args([
                    "run",
                    "-i",
                    "--rm",
                    "--network=host",
                    "--cap-add=NET_ADMIN",
                    "--cap-add=NET_RAW",
                ]);
                c.args(["--entrypoint", "claude"]);
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
                c.args(claude_args);
                c
            } else {
                let mut c = Command::new("claude");
                c.args(claude_args);
                c
            }
        }
        Backend::OpenCode => {
            let model = config.model.as_ref().expect("model must be set for OpenCode backend");
            let mut c = Command::new("opencode");
            c.args([
                "run",
                "--format",
                "json",
                "--model",
                model,
                prompt,
            ]);
            c
        }
    }
}

/// Spawn an agent process, stream its output, kill it when streaming ends,
/// and wait for exit.  Returns `(success, iteration_cost)`.
///
/// The child is stored in `runner` for cleanup-on-drop safety.
/// After `filter_stream` returns (normally, broken pipe, or interrupt)
/// the child is killed before waiting — this is a no-op if it already
/// exited but prevents deadlock if it ignores SIGPIPE.
fn invoke_process(
    runner: &mut LoopRunner,
    config: &Config,
    prompt: &str,
    label: &str,
    log_prefix: &str,
    iteration: u32,
    prior_total: f64,
) -> (bool, f64) {
    let backend = config.backend();
    let in_container = config.image.is_some() && backend == Backend::Claude;
    
    log(&format!(
        "Launching {} {}(iteration {})...",
        label,
        if in_container { "in container " } else { "" },
        iteration
    ));

    let mut cmd = build_command(config, prompt);

    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let bin = match backend {
                Backend::Claude if config.image.is_some() => "docker",
                Backend::Claude => "claude",
                Backend::OpenCode => "opencode",
            };
            log_error(&format!("failed to spawn '{}': {}", bin, e));
            return (false, 0.0);
        }
    };

    signal::set_child_pid(child.id() as i32);

    let log_path = config.log_dir.as_ref().map(|dir| {
        PathBuf::from(dir).join(format!("{}-{}.jsonl", log_prefix, iteration))
    });

    let mut iter_cost = 0.0;
    if let Some(stdout) = child.stdout.take() {
        runner.child = Some(child);
        iter_cost = match backend {
            Backend::Claude => stream::filter_stream(stdout, log_path.as_deref(), prior_total),
            Backend::OpenCode => stream_opencode::filter_stream(stdout, log_path.as_deref(), prior_total),
        };
        // Kill the child after streaming ends — prevents deadlock if it
        // ignores SIGPIPE and keeps writing.  No-op if already exited.
        if let Some(ref mut ch) = runner.child {
            let _ = ch.kill();
        }
        if signal::interrupted() {
            return (false, iter_cost);
        }
    } else {
        runner.child = Some(child);
    }

    let status = if let Some(ref mut child) = runner.child {
        match child.wait() {
            Ok(s) => {
                if signal::interrupted() {
                    return (false, iter_cost);
                }
                s.success()
            }
            Err(e) => {
                log_error(&format!("failed to wait on {} process: {}", label, e));
                false
            }
        }
    } else {
        false
    };

    signal::set_child_pid(0);
    runner.child = None;

    log(&format!(
        "{} exited ({})",
        label,
        if status { "success" } else { "failure" }
    ));
    (status, iter_cost)
}

/// Invoke the agent, piping stdout through the stream filter.
/// Returns `(success, iteration_cost)`.
fn invoke_agent(runner: &mut LoopRunner, config: &Config, iteration: u32, prior_total: f64) -> (bool, f64) {
    let prompt = "Read .loop/protocol.md and follow its instructions.";
    let label = match config.backend() {
        Backend::Claude => "Claude",
        Backend::OpenCode => "OpenCode",
    };
    invoke_process(runner, config, prompt, label, "iteration", iteration, prior_total)
}

/// Render a full-width box-drawn banner for the judge verdict.
/// Includes the first ~2 lines of the verdict reason text.
///
/// ```text
/// ╔══════════════════════════════════════╗
/// ║        JUDGE VERDICT: PASS          ║
/// ╠══════════════════════════════════════╣
/// ║  "Tests pass and all features are   ║
/// ║   present in the source code"       ║
/// ╚══════════════════════════════════════╝
/// ```
fn render_judge_banner(pass: bool, content: &str) {
    let inner_w = 38; // inner width of the box (characters between ║ borders)
    let verdict_label = if pass { "PASS" } else { "FAIL" };
    let color = if pass { GREEN } else { RED };

    // Build header line: center "JUDGE VERDICT: PASS/FAIL"
    let title = format!("JUDGE VERDICT: {}", verdict_label);
    let padding = if inner_w > title.len() {
        inner_w - title.len()
    } else {
        0
    };
    let left_pad = padding / 2;
    let right_pad = padding - left_pad;
    let title_line = format!(
        "{:>w_left$}{}{:>w_right$}",
        "",
        title,
        "",
        w_left = left_pad,
        w_right = right_pad
    );

    // Extract reason text: skip the first line (VERDICT: ...) and take up to 2 non-empty lines
    let reason_lines: Vec<&str> = content
        .lines()
        .skip(1)
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(2)
        .collect();

    let bar = "═".repeat(inner_w);

    eprintln!();
    eprintln!("{}{}╔{}╗{}", BOLD, color, bar, RESET);
    eprintln!("{}{}║{}║{}", BOLD, color, title_line, RESET);

    if !reason_lines.is_empty() {
        eprintln!("{}{}╠{}╣{}", BOLD, color, bar, RESET);
        for reason in &reason_lines {
            // Truncate and pad reason to fit within inner_w, with 2-char left margin
            let max_text = inner_w - 4; // 2 left margin + 2 right margin
            let text: std::borrow::Cow<str> = if reason.chars().count() > max_text {
                std::borrow::Cow::Owned(reason.chars().take(max_text).collect())
            } else {
                std::borrow::Cow::Borrowed(reason)
            };
            let text_right_pad = inner_w - 2 - text.chars().count();
            eprintln!(
                "{}{}║  {}{}{}{}║{}",
                BOLD,
                color,
                RESET,
                text,
                " ".repeat(text_right_pad),
                color,
                RESET
            );
        }
    }

    eprintln!("{}{}╚{}╝{}", BOLD, color, bar, RESET);
    eprintln!();
}

/// Invoke the judge — a fresh agent with zero implementation context.
/// Returns true if the judge's verdict is PASS.
fn invoke_judge(runner: &mut LoopRunner, config: &Config, iteration: u32) -> bool {
    let judge_prompt = "\
Read .loop/judge.md. It describes what to verify.
Test the feature exactly as described. You have full shell access.
When done, write your verdict to .loop/verdict.md in this exact format:

VERDICT: PASS

<what you tested and what happened>

or

VERDICT: FAIL

<what you tested, what went wrong, and what the correct behavior should be>";

    let (status, _) = invoke_process(runner, config, judge_prompt, "judge", "judge", iteration, 0.0);
    if !status {
        return false;
    }

    // Read verdict
    match fs::read_to_string(VERDICT_PATH) {
        Ok(content) => {
            let pass = content.starts_with("VERDICT: PASS");
            render_judge_banner(pass, &content);
            pass
        }
        Err(e) => {
            log_error(&format!("failed to read {}: {}", VERDICT_PATH, e));
            false
        }
    }
}

/// Render boundary violations as a compact file tree with colored markers.
/// Violations are strings like "DENY: src/foo.rs (no matching rule)".
/// Allowed-scope files get a green ✓, violations get a red ✗.
#[allow(clippy::string_slice)] // slices at ASCII delimiter positions from .find()
fn render_violation_tree(violations: &[String]) {
    use std::collections::BTreeMap;

    // Parse each violation into (file_path, reason)
    let mut entries: Vec<(&str, &str)> = Vec::new();
    for v in violations {
        // Format: "TAG: path (reason)" — extract path after first ": " and before " ("
        if let Some(colon_pos) = v.find(": ") {
            let rest = &v[colon_pos + 2..];
            let (path, reason) = if let Some(paren_pos) = rest.find(" (") {
                (&rest[..paren_pos], &rest[paren_pos + 1..])
            } else {
                (rest, "")
            };
            entries.push((path.trim(), reason.trim()));
        }
    }

    if entries.is_empty() {
        return;
    }

    // Group files by their directory (or root "." for top-level files)
    let mut tree: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (path, reason) in &entries {
        if let Some(slash_pos) = path.rfind('/') {
            let dir = &path[..slash_pos];
            let file = &path[slash_pos + 1..];
            tree.entry(dir).or_default().push((file, reason));
        } else {
            tree.entry(".").or_default().push((path, reason));
        }
    }

    // Render tree
    let dirs: Vec<_> = tree.keys().copied().collect();
    let dir_count = dirs.len();
    for (di, dir) in dirs.iter().enumerate() {
        let is_last_dir = di + 1 == dir_count;
        let dir_prefix = if is_last_dir { "└── " } else { "├── " };
        let child_prefix = if is_last_dir { "    " } else { "│   " };

        eprintln!("  {}{}{}/{}", dir_prefix, DIM, dir, RESET);

        let files = &tree[dir];
        let file_count = files.len();
        for (fi, (file, reason)) in files.iter().enumerate() {
            let is_last_file = fi + 1 == file_count;
            let file_conn = if is_last_file { "└── " } else { "├── " };
            eprintln!(
                "  {}{}{}✗{} {} {}{}{}",
                child_prefix, file_conn, RED, RESET, file, DIM, reason, RESET
            );
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
        render_violation_tree(&boundary.violations);

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

    // Render guard results as a box-drawn table with timing
    if !guard_results.is_empty() {
        // Column widths (visual characters, not bytes)
        let cmd_w = guard_results.iter().map(|r| r.name.len()).max().unwrap_or(10).max(10);
        let stat_w = 8; // enough for "SKIPPED" + padding
        let time_w = 6; // e.g. "  12s "

        let top    = format!("  ┌{}┬{}┬{}┐", "─".repeat(cmd_w + 2), "─".repeat(stat_w), "─".repeat(time_w + 1));
        let bottom = format!("  └{}┴{}┴{}┘", "─".repeat(cmd_w + 2), "─".repeat(stat_w), "─".repeat(time_w + 1));

        eprintln!("{}", top);
        for r in &guard_results {
            // Command column: left-aligned, padded to cmd_w
            let cmd_padded = format!("{:<w$}", r.name, w = cmd_w);

            // Status column: colored, centered in stat_w characters
            let status_cell = if r.skipped {
                format!("{}SKIPPED {}", ORANGE, RESET)
            } else if r.passed {
                format!(" {} PASS {}  ", GREEN, RESET)
            } else {
                format!(" {} FAIL {}  ", RED, RESET)
            };

            // Time column
            let time_cell = if r.skipped {
                format!("  -  ")
            } else {
                let secs = r.elapsed_secs.round() as u64;
                format!("{:>3}s ", secs)
            };

            eprintln!("  │ {} │{}│ {} │", cmd_padded, status_cell, time_cell);
        }
        eprintln!("{}", bottom);
    }

    all_passed
}

fn print_clean_help() {
    eprintln!(
        "{}{}[yoke clean]{} reset .loop/ working files",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke clean",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("Clears working files back to a blank slate:");
    eprintln!("  plan.md, notes.md, verdict.md, guard-results.md  →  emptied");
    eprintln!("  saga-notes.md, decisions.md, sub-plan.md          →  emptied (if present)");
    eprintln!("  judge.md  →  reset to default template");
    eprintln!();
    eprintln!("Structural files are left untouched:");
    eprintln!("  protocol.md, saga-protocol.md, yoke.conf, briefing.md, specification.md");
    eprintln!();
    eprintln!("Mode snapshots in .loop/.modes/ are preserved by clean.");
    eprintln!();
    eprintln!("Non-empty working files are auto-stashed to .loop/.stash/ before wiping.");
    eprintln!("Browse with: {}yoke stash log{}   Recover with: {}yoke stash pop{}", BOLD, RESET, BOLD, RESET);
}

fn clean() -> i32 {
    let loop_dir = Path::new(".loop");
    if !loop_dir.exists() {
        log_error(".loop/ directory not found — nothing to clean");
        return 1;
    }

    // Stash non-empty working files before wiping
    let stashed = stash_working_files();

    log("Cleaning .loop/ working files...");

    // Clear working files to empty
    for path in &[
        PLAN_PATH, NOTES_PATH, VERDICT_PATH, GUARD_RESULTS_PATH,
        SAGA_NOTES_PATH, DECISIONS_PATH, SUB_PLAN_PATH,
    ] {
        let p = Path::new(path);
        if p.exists() {
            if let Err(e) = fs::write(p, "") {
                log_error(&format!("failed to clear {}: {}", path, e));
                return 1;
            }
            log(&format!("cleared: {}", path));
        }
    }

    // Reset judge.md to default template (saga uses a different default)
    let judge = Path::new(JUDGE_PATH);
    if judge.exists() {
        let is_saga = Path::new(SPECIFICATION_PATH).exists();
        let template = if is_saga { DEFAULT_SAGA_JUDGE } else { DEFAULT_JUDGE };
        if let Err(e) = fs::write(judge, template) {
            log_error(&format!("failed to reset {}: {}", JUDGE_PATH, e));
            return 1;
        }
        log(&format!("reset: {} (default template)", JUDGE_PATH));
    }

    log("Clean complete");
    if stashed > 0 {
        log(&format!(
            "stashed {} file(s) — browse with: yoke stash log",
            stashed
        ));
    }
    0
}

// ── Stash helpers ──────────────────────────────────────────────────────

/// Format Unix epoch seconds as `YYYY-MM-DDThh:mm:ss` using Hinnant's algorithm.
fn format_unix_timestamp(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let min = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, h, min, s)
}

/// SipHash timestamp + file names/contents → lower 28 bits → 7-char hex.
fn generate_stash_hash(timestamp: &str, files: &[(String, Vec<u8>)]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    timestamp.hash(&mut hasher);
    for (name, contents) in files {
        name.hash(&mut hasher);
        contents.hash(&mut hasher);
    }
    format!("{:07x}", hasher.finish() & 0x0FFF_FFFF)
}

/// Collect all regular files in `.loop/` excluding dotfile/dotdir entries.
/// Returns sorted `(filename, contents)` pairs for deterministic hashing.
fn collect_stashable_files() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(".loop") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_file() {
                if let Ok(contents) = fs::read(&path) {
                    files.push((name, contents));
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

struct StashEntry {
    hash: String,
    timestamp: String,
    mode: String,
    files: Vec<String>,
}

/// Parse `.loop/.stash/index` into a list of stash entries (oldest first).
fn parse_stash_index() -> Vec<StashEntry> {
    let index_path = format!("{}/index", STASH_DIR);
    let content = match fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() < 4 {
                return None;
            }
            Some(StashEntry {
                hash: parts[0].to_string(),
                timestamp: parts[1].to_string(),
                mode: parts[2].to_string(),
                files: parts[3].split(',').map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

// ── Stash core ─────────────────────────────────────────────────────────

/// Snapshot all stashable files in `.loop/` to a new log entry.
/// Returns `Ok(hash)` on success.
fn stash_snapshot() -> Result<String, String> {
    let files = collect_stashable_files();
    if files.is_empty() {
        return Err("no files to stash in .loop/".to_string());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let timestamp = format_unix_timestamp(now);
    let mode = detect_mode().unwrap_or("unknown").to_string();

    let mut hash = generate_stash_hash(&timestamp, &files);

    // Collision handling: rehash with counter suffix
    let stash_base = Path::new(STASH_DIR);
    for attempt in 0..100u32 {
        if !stash_base.join(&hash).exists() {
            break;
        }
        if attempt == 99 {
            return Err("hash collision after 100 attempts".to_string());
        }
        hash = generate_stash_hash(&format!("{}{}", timestamp, attempt + 1), &files);
    }

    let entry_dir = stash_base.join(&hash);
    fs::create_dir_all(&entry_dir)
        .map_err(|e| format!("failed to create {}: {}", entry_dir.display(), e))?;

    let file_names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();

    for (name, contents) in &files {
        let dest = entry_dir.join(name);
        fs::write(&dest, contents)
            .map_err(|e| format!("failed to write {}: {}", dest.display(), e))?;
    }

    // Append to index
    let index_path = format!("{}/index", STASH_DIR);
    let line = format!(
        "{}|{}|{}|{}\n",
        hash,
        timestamp,
        mode,
        file_names.join(",")
    );
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .map_err(|e| format!("failed to open index: {}", e))?;
    std::io::Write::write_all(&mut f, line.as_bytes())
        .map_err(|e| format!("failed to write index: {}", e))?;

    Ok(hash)
}

// ── Stash commands ─────────────────────────────────────────────────────

/// Snapshot `.loop/` to a new stash log entry. Returns file count for `clean()`.
fn stash_working_files() -> usize {
    let files = collect_stashable_files();
    if files.is_empty() {
        return 0;
    }
    let count = files.len();
    match stash_snapshot() {
        Ok(_) => count,
        Err(_) => 0,
    }
}

/// `yoke stash` — create a new stash entry and print the hash.
fn stash_create() -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    match stash_snapshot() {
        Ok(hash) => {
            log(&format!("stashed → {}{}{}", BLUE, hash, RESET));
            0
        }
        Err(msg) => {
            log_error(&msg);
            1
        }
    }
}

/// `yoke stash log` — list all stash entries (newest first).
fn stash_log_cmd() -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    let entries = parse_stash_index();
    if entries.is_empty() {
        log("no stash entries");
        return 0;
    }
    for entry in entries.iter().rev() {
        eprintln!(
            "{}{}{}{}  {}  mode={}",
            BOLD, BLUE, entry.hash, RESET, entry.timestamp, entry.mode
        );
        for file in &entry.files {
            eprintln!("    {}", file);
        }
    }
    0
}

/// `yoke stash checkout <hash>` — auto-stash current state, clear `.loop/` files,
/// restore target entry. Supports prefix matching.
fn stash_checkout(target_hash: &str) -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    let entries = parse_stash_index();
    if entries.is_empty() {
        log_error("no stash entries");
        return 1;
    }

    // Find matching entries (exact or prefix)
    let matches: Vec<&StashEntry> = entries
        .iter()
        .filter(|e| e.hash == target_hash || e.hash.starts_with(target_hash))
        .collect();

    match matches.len() {
        0 => {
            log_error(&format!("no stash entry matching '{}'", target_hash));
            1
        }
        1 => {
            let target = matches[0];
            let entry_dir = Path::new(STASH_DIR).join(&target.hash);
            if !entry_dir.exists() {
                log_error("stash entry directory missing — index is corrupt");
                return 1;
            }

            // Auto-stash current state (skip if .loop/ has no stashable files)
            let current_files = collect_stashable_files();
            if !current_files.is_empty() {
                match stash_snapshot() {
                    Ok(hash) => {
                        log(&format!(
                            "auto-stashed current state → {}{}{}",
                            BLUE, hash, RESET
                        ));
                    }
                    Err(msg) => {
                        log_error(&format!("failed to auto-stash: {}", msg));
                        return 1;
                    }
                }
            }

            // Remove all stashable files from .loop/
            for (name, _) in &collect_stashable_files() {
                let path = Path::new(".loop").join(name);
                let _ = fs::remove_file(&path);
            }

            // Restore files from the target entry
            if let Ok(dir_entries) = fs::read_dir(&entry_dir) {
                for entry in dir_entries.flatten() {
                    let name = entry.file_name();
                    let dest = Path::new(".loop").join(&name);
                    if let Err(e) = fs::copy(entry.path(), &dest) {
                        log_error(&format!(
                            "failed to restore {}: {}",
                            name.to_string_lossy(),
                            e
                        ));
                        return 1;
                    }
                }
            }

            log(&format!("checked out {}{}{}", BLUE, target.hash, RESET));
            0
        }
        n => {
            log_error(&format!(
                "ambiguous hash '{}' — matches {} entries",
                target_hash, n
            ));
            1
        }
    }
}

/// `yoke stash pop` — checkout the most recent stash entry.
fn stash_pop() -> i32 {
    let entries = parse_stash_index();
    match entries.last() {
        Some(entry) => stash_checkout(&entry.hash),
        None => {
            log_error("no stash found — nothing to restore");
            1
        }
    }
}

fn print_stash_help() {
    eprintln!(
        "{}{}[yoke stash]{} snapshot and restore .loop/ state",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} yoke stash [subcommand]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}SUBCOMMANDS:{}", BOLD, RESET);
    eprintln!(
        "  {}(none){}           Snapshot all .loop/ files to a new stash entry",
        BOLD, RESET
    );
    eprintln!(
        "  {}log{}              List all stash entries (hash, timestamp, mode, files)",
        BOLD, RESET
    );
    eprintln!(
        "  {}checkout <hash>{}  Auto-stash current state, then restore target entry",
        BOLD, RESET
    );
    eprintln!(
        "  {}pop{}              Alias for checkout of the most recent entry",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}EXAMPLES:{}", BOLD, RESET);
    eprintln!("  yoke stash                   # snapshot current .loop/ files");
    eprintln!("  yoke stash log               # show all stash entries");
    eprintln!("  yoke stash checkout a1b2c3d  # restore a specific snapshot");
    eprintln!("  yoke stash pop               # restore the most recent snapshot");
}

/// Return the list of (path, default_content) pairs for a given mode.
fn mode_files(mode: &str) -> Option<Vec<(&'static str, &'static str)>> {
    Some(match mode {
        "loop" => vec![
            (CONF_PATH, DEFAULT_YOKE_CONF),
            (PROTOCOL_PATH, DEFAULT_PROTOCOL),
            (BRIEFING_PATH, DEFAULT_BRIEFING),
            (PLAN_PATH, ""),
            (NOTES_PATH, ""),
            (GUARD_RESULTS_PATH, ""),
        ],
        "brute" => vec![
            (CONF_PATH, DEFAULT_BRUTE_CONF),
            (PROTOCOL_PATH, DEFAULT_BRUTE_PROTOCOL),
            (BRIEFING_PATH, DEFAULT_BRIEFING),
            (PLAN_PATH, ""),
            (JUDGE_PATH, DEFAULT_JUDGE),
            (NOTES_PATH, ""),
            (VERDICT_PATH, ""),
            (GUARD_RESULTS_PATH, ""),
        ],
        "saga" => vec![
            (CONF_PATH, DEFAULT_SAGA_CONF),
            (SAGA_PROTOCOL_PATH, DEFAULT_SAGA_PROTOCOL),
            (PROTOCOL_PATH, DEFAULT_SAGA_WORKER_PROTOCOL),
            (JUDGE_PATH, DEFAULT_SAGA_JUDGE),
            (SPECIFICATION_PATH, ""),
            (SAGA_NOTES_PATH, ""),
            (DECISIONS_PATH, ""),
            (SUB_PLAN_PATH, ""),
            (NOTES_PATH, ""),
            (VERDICT_PATH, ""),
            (GUARD_RESULTS_PATH, ""),
        ],
        _ => return None,
    })
}

/// Detect the currently active mode from .loop/ contents.
fn detect_mode() -> Option<&'static str> {
    if !Path::new(".loop").exists() {
        return None;
    }
    if Path::new(SPECIFICATION_PATH).exists() {
        Some("saga")
    } else if Path::new(JUDGE_PATH).exists() {
        Some("brute")
    } else if Path::new(PROTOCOL_PATH).exists() {
        Some("loop")
    } else {
        None
    }
}

/// Backup current mode's files into `.loop/.modes/<current>/`, then either
/// restore from a prior snapshot of the target mode or do a fresh init.
fn switch_mode(current: &str, target: &str) -> i32 {
    use std::collections::HashSet;

    let current_files = mode_files(current).unwrap();
    let target_files = mode_files(target).unwrap();

    let current_paths: HashSet<&str> = current_files.iter().map(|(p, _)| *p).collect();
    let target_paths: HashSet<&str> = target_files.iter().map(|(p, _)| *p).collect();

    // 1. Snapshot current mode's files into .loop/.modes/<current>/
    let snapshot_dir = PathBuf::from(format!(".loop/.modes/{}", current));
    if let Err(e) = fs::create_dir_all(&snapshot_dir) {
        log_error(&format!("failed to create {}: {}", snapshot_dir.display(), e));
        return 1;
    }

    for path in &current_paths {
        let src = Path::new(path);
        if src.exists() {
            let dest = snapshot_dir.join(src.file_name().unwrap());
            if let Err(e) = fs::copy(src, &dest) {
                log_error(&format!("failed to snapshot {}: {}", path, e));
                return 1;
            }
            log(&format!("snapshot: {} → {}", path, dest.display()));
        }
    }

    // 2. Delete files exclusive to current mode (not in target)
    for path in current_paths.difference(&target_paths) {
        let p = Path::new(path);
        if p.exists() {
            if let Err(e) = fs::remove_file(p) {
                log_error(&format!("failed to remove {}: {}", path, e));
                return 1;
            }
            log(&format!("removed: {}", path));
        }
    }

    // 3. Check for a prior snapshot of the target mode
    let restore_dir = PathBuf::from(format!(".loop/.modes/{}", target));
    if restore_dir.exists() {
        log(&format!("Restoring '{}' mode from snapshot...", target));
        for (path, _) in &target_files {
            let file_name = Path::new(path).file_name().unwrap();
            let src = restore_dir.join(file_name);
            if src.exists() {
                if let Err(e) = fs::copy(&src, path) {
                    log_error(&format!("failed to restore {}: {}", path, e));
                    return 1;
                }
                log(&format!("restored: {}", path));
            } else {
                // File wasn't in the snapshot — create from template if missing
                let p = Path::new(path);
                if !p.exists() {
                    let (_, content) = target_files.iter().find(|(tp, _)| tp == path).unwrap();
                    if let Err(e) = fs::write(p, content) {
                        log_error(&format!("failed to write {}: {}", path, e));
                        return 1;
                    }
                    log(&format!("created: {}", path));
                }
            }
        }
        // Remove the restored snapshot directory
        if let Err(e) = fs::remove_dir_all(&restore_dir) {
            log_error(&format!("failed to remove {}: {}", restore_dir.display(), e));
            return 1;
        }
    } else {
        // 4. Fresh init for the target mode
        log(&format!("Fresh init for '{}' mode...", target));
        for (path, content) in &target_files {
            let p = Path::new(path);
            if p.exists() {
                // Shared file that wasn't deleted — overwrite with target template
                // (e.g. protocol.md, yoke.conf have different content per mode)
                if current_paths.contains(path) {
                    if let Err(e) = fs::write(p, content) {
                        log_error(&format!("failed to write {}: {}", path, e));
                        return 1;
                    }
                    log(&format!("overwritten: {} (new mode template)", path));
                } else {
                    log(&format!("skip (exists): {}", path));
                }
            } else {
                if let Err(e) = fs::write(p, content) {
                    log_error(&format!("failed to write {}: {}", path, e));
                    return 1;
                }
                log(&format!("created: {}", path));
            }
        }
    }

    log(&format!("Switched from '{}' to '{}' mode", current, target));
    0
}

fn init(mode: &str) -> i32 {
    let files = match mode_files(mode) {
        Some(f) => f,
        None => {
            log_error(&format!("unknown mode '{}'", mode));
            return 2;
        }
    };

    // Detect current mode — if different, do a backup/restore switch
    match detect_mode() {
        Some(current) if current != mode => {
            log(&format!("Switching from '{}' to '{}' mode...", current, mode));
            return switch_mode(current, mode);
        }
        _ => {} // fresh init or same-mode idempotent
    }

    let loop_dir = Path::new(".loop");
    if !loop_dir.exists() {
        if let Err(e) = fs::create_dir(loop_dir) {
            log_error(&format!("failed to create .loop/: {}", e));
            return 1;
        }
        log("Created .loop/");
    }

    log(&format!("Initializing '{}' mode...", mode));

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

/// Core plan-loop runner that can be called standalone or nested inside brute.
///
/// - `config`: already-loaded Config
/// - `plan_path`: path to the plan file (e.g. PLAN_PATH or SUB_PLAN_PATH)
/// - `dry_run`: if true, skip Claude invocation (one iteration only)
/// - `nested`: if true, running inside brute (adjusts output banners)
///
/// Returns Ok(()) on success (STATUS: DONE + guards pass), Err(i32) with exit code on failure.
fn run_plan_loop(config: &Config, plan_path: &str, dry_run: bool, nested: bool) -> Result<(), i32> {
    // Preflight — skip guard-results clear when nested (brute handles it)
    preflight(plan_path, !nested);
    log("Preflight OK");

    // Build protected files list dynamically based on the plan path
    let protected: Vec<&str> = vec![PROTOCOL_PATH, plan_path, CONF_PATH];

    // Backup protected files and create the runner (Drop handles cleanup)
    let backup_dir = backup_files(&protected);
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut iteration: u32 = 0;
    let mut total_cost: f64 = 0.0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return Err(130);
        }
        iteration += 1;
        eprintln!();
        if nested {
            eprintln!(
                "{}{}┌──────────────────────────────────────┐{}",
                BOLD, ORANGE, RESET
            );
            eprintln!(
                "{}{}│      Plan Iteration {:>4}             │{}",
                BOLD, ORANGE, iteration, RESET
            );
            eprintln!(
                "{}{}└──────────────────────────────────────┘{}",
                BOLD, ORANGE, RESET
            );
        } else {
            eprintln!(
                "{}{}╔══════════════════════════════════════╗{}",
                BOLD, ORANGE, RESET
            );
            eprintln!(
                "{}{}║         Iteration {:>4}              ║{}",
                BOLD, ORANGE, iteration, RESET
            );
            eprintln!(
                "{}{}╚══════════════════════════════════════╝{}",
                BOLD, ORANGE, RESET
            );
        }

        // Show stage progress bar if plan has parseable stages
        if let Some((completed, total)) = stage_progress(plan_path) {
            eprintln!("{}", format_progress_bar(completed, total));
        }

        // Restore protected files
        restore_files(&runner.backup_dir, &protected);

        // NOTE: Do NOT clear guard-results.md here. The previous iteration's
        // guard results must persist so Claude can read them and fix failures.
        // The file is initialized to empty in preflight() (first iteration)
        // and overwritten by run_all_guards() after Claude finishes.

        if dry_run {
            log(&format!(
                "(dry-run) Skipping {} invocation",
                match config.backend() {
                    Backend::Claude => "Claude",
                    Backend::OpenCode => "OpenCode",
                }
            ));
        } else {
            let (success, iter_cost) = invoke_agent(&mut runner, config, iteration, total_cost);
            total_cost += iter_cost;
            if !success {
                log_error("Claude invocation failed — aborting loop");
                return Err(1);
            }
        }

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return Err(130);
        }

        // Run guards
        let guards_passed = run_all_guards(config);

        if guards_passed {
            log(&format!(
                "{}{}All guards passed{}",
                GREEN, BOLD, RESET
            ));

            if dry_run {
                log("(dry-run) Guards passed \u{2014} exiting after one iteration");
                return Ok(());
            }

            if is_done() {
                eprintln!();
                eprintln!(
                    "{}{}  STATUS: DONE and all guards pass \u{2014} loop complete  {}",
                    GREEN, BOLD, RESET
                );
                eprintln!();
                return Ok(());
            } else {
                log("Guards passed but STATUS is not DONE \u{2014} continuing");
            }
        } else {
            log(&format!(
                "{}Some guards failed \u{2014} Claude will see results next iteration{}",
                ORANGE, RESET
            ));

            if dry_run {
                log("(dry-run) Exiting after one iteration");
                return Err(1);
            }
        }
    }
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
        "Config loaded: max_tail={}, {} scope rules, {} guards, backend={}{}",
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len(),
        match config.backend() {
            Backend::Claude => "claude",
            Backend::OpenCode => config.model.as_ref().map_or("opencode", |m| m.as_str()),
        },
        config.image.as_ref().map_or(
            String::from(", sandbox=off"),
            |img| format!(", image={}", img)
        )
    ));

    match run_plan_loop(&config, PLAN_PATH, dry_run, false) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// Result of a brute loop run.
enum BruteResult {
    /// Judge said PASS — feature is complete.
    Pass,
    /// 3 consecutive judge FAILs — bailing out for re-scoping.
    Bailout,
    /// User interrupted (SIGINT).
    Interrupt,
    /// Hard error (spawn failure, missing files, etc.).
    Error,
}

/// Protected files for the brute runner.
const BRUTE_PROTECTED_FILES: &[&str] = &[PROTOCOL_PATH, JUDGE_PATH, CONF_PATH];

/// Max consecutive judge failures before the brute loop bails out.
const MAX_JUDGE_FAILURES: u32 = 3;

fn run_brute_inner(dry_run: bool) -> BruteResult {
    // Load config
    let config = match Config::load(Path::new(CONF_PATH)) {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    };

    log(&format!(
        "Config loaded: mode=brute, max_tail={}, {} scope rules, {} guards{}",
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len(),
        config.image.as_ref().map_or(
            String::from(", sandbox=off"),
            |img| format!(", image={}", img)
        )
    ));

    // Preflight: require protocol.md, judge.md, yoke.conf
    for path in &[PROTOCOL_PATH, JUDGE_PATH, CONF_PATH] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }

    // Fail fast if critical content files are empty
    for path in &[JUDGE_PATH] {
        match fs::read_to_string(path) {
            Ok(content) if content.trim().is_empty() => {
                log_error(&format!("{} is empty — fill it in before running", path));
                process::exit(1);
            }
            _ => {}
        }
    }

    // Create notes.md and verdict.md if missing
    for path in &[NOTES_PATH, VERDICT_PATH] {
        if !Path::new(path).exists()
            && let Err(e) = fs::write(path, "")
        {
            log_error(&format!("cannot create {}: {}", path, e));
            process::exit(1);
        }
    }

    // Clear verdict and guard results for the first brute attempt.
    // Subsequent retries preserve these so the worker can read feedback.
    let _ = fs::write(VERDICT_PATH, "");
    let _ = fs::write(GUARD_RESULTS_PATH, "");

    log("Preflight OK");

    run_brute_core(&config, PLAN_PATH, dry_run)
}

/// Core brute loop logic, parameterized by plan path. Used by both standalone
/// brute mode and saga mode (which passes SUB_PLAN_PATH).
///
/// - `config`: already-loaded Config
/// - `plan_path`: path to the plan file
/// - `dry_run_inner`: if true, skip Claude invocation (one iteration only)
fn run_brute_core(config: &Config, plan_path: &str, dry_run: bool) -> BruteResult {
    // Backup protected files
    let backup_dir = backup_files(BRUTE_PROTECTED_FILES);
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut iteration: u32 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return BruteResult::Interrupt;
        }
        iteration += 1;
        eprintln!();
        eprintln!(
            "{}{}╔══════════════════════════════════════╗{}",
            BOLD, ORANGE, RESET
        );
        eprintln!(
            "{}{}║     Brute Iteration {:>4}             ║{}",
            BOLD, ORANGE, iteration, RESET
        );
        eprintln!(
            "{}{}╚══════════════════════════════════════╝{}",
            BOLD, ORANGE, RESET
        );

        // Restore protected files
        restore_files(&runner.backup_dir, BRUTE_PROTECTED_FILES);

        // NOTE: Do NOT clear verdict.md or guard-results.md here.
        // On retry after judge FAIL, the worker needs to see the previous
        // verdict and guard feedback. Both files are cleared once in
        // run_brute_inner() (first attempt) and overwritten by the judge /
        // guards each iteration.

        if dry_run {
            log("(dry-run) Skipping worker invocation");
        } else {
            log("Using plan runner as worker...");
            match run_plan_loop(config, plan_path, false, true) {
                Ok(()) => log("Plan runner completed successfully"),
                Err(130) => {
                    log("Interrupted \u{2014} shutting down");
                    return BruteResult::Interrupt;
                }
                Err(_) => {
                    log_error("Plan runner failed — aborting brute loop");
                    return BruteResult::Error;
                }
            }
        }

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return BruteResult::Interrupt;
        }

        // Run guards (skip when not dry-run — the plan loop already ran them)
        if dry_run {
            let passed = run_all_guards(config);
            if !passed {
                log(&format!(
                    "{}Guards failed{}", ORANGE, RESET
                ));
                log("(dry-run) Exiting after one iteration");
                return BruteResult::Error;
            }
            log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));
            log("(dry-run) Guards passed \u{2014} exiting after one iteration");
            return BruteResult::Pass;
        }

        // Guards passed — invoke judge
        eprintln!();
        eprintln!(
            "{}{}┌─ Judge ────────────────────────────────┐{}",
            BOLD, BLUE, RESET
        );
        eprintln!(
            "{}{}│  Invoking judge (attempt {:>4})          │{}",
            BOLD, BLUE, iteration, RESET
        );
        eprintln!(
            "{}{}└────────────────────────────────────────┘{}",
            BOLD, BLUE, RESET
        );
        let pass = invoke_judge(&mut runner, config, iteration);

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return BruteResult::Interrupt;
        }

        if pass {
            eprintln!();
            eprintln!(
                "{}{}  Judge says PASS \u{2014} brute loop complete  {}",
                GREEN, BOLD, RESET
            );
            eprintln!();
            return BruteResult::Pass;
        }

        consecutive_failures += 1;
        log(&format!(
            "{}Judge says FAIL ({}/{}) \u{2014} {}{}",
            ORANGE,
            consecutive_failures,
            MAX_JUDGE_FAILURES,
            if consecutive_failures >= MAX_JUDGE_FAILURES {
                "bailing out"
            } else {
                "worker will see verdict next iteration"
            },
            RESET
        ));

        if consecutive_failures >= MAX_JUDGE_FAILURES {
            eprintln!();
            eprintln!(
                "{}{}  {} consecutive judge FAILs \u{2014} bailing out  {}",
                RED, BOLD, MAX_JUDGE_FAILURES, RESET
            );
            eprintln!();
            return BruteResult::Bailout;
        }

        // Reset STATUS to IN_PROGRESS so the plan runner re-executes on the
        // next brute attempt, but preserve all notes.
        reset_notes_status();
    }
}

/// Top-level brute runner that maps BruteResult to an exit code.
fn run_brute(dry_run: bool) -> i32 {
    match run_brute_inner(dry_run) {
        BruteResult::Pass => 0,
        BruteResult::Bailout => 1,
        BruteResult::Interrupt => 130,
        BruteResult::Error => 1,
    }
}

/// Check the first line of saga-notes.md for STATUS: DONE.
fn is_saga_done() -> bool {
    match fs::read_to_string(SAGA_NOTES_PATH) {
        Ok(content) => content.starts_with("STATUS: DONE"),
        Err(_) => false,
    }
}

/// Invoke the scoper (Agent 1) — a fresh LLM session that reads the spec,
/// writes sub-plan.md, and updates saga-notes.md.
/// Returns true if the invocation succeeded.
fn invoke_scoper(runner: &mut LoopRunner, config: &Config, cycle: u32) -> bool {
    let scoper_prompt = "Read .loop/saga-protocol.md and follow its instructions.";
    let (status, _) = invoke_process(
        runner, config, scoper_prompt, "scoper", "scoper", cycle, 0.0,
    );
    status
}

/// Run the saga loop: scoper (Agent 1) decomposes the spec into sub-plans,
/// each sub-plan is run through the brute loop (Agent 2 + Agent 3).
fn run_saga(dry_run: bool) -> i32 {
    // Load config
    let config = match Config::load(Path::new(CONF_PATH)) {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    };

    log(&format!(
        "Config loaded: mode=saga, max_tail={}, {} scope rules, {} guards{}",
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len(),
        config.image.as_ref().map_or(
            String::from(", sandbox=off"),
            |img| format!(", image={}", img)
        )
    ));

    // Preflight: verify required saga files exist
    for path in &[
        SPECIFICATION_PATH,
        SAGA_PROTOCOL_PATH,
        PROTOCOL_PATH,
        JUDGE_PATH,
        CONF_PATH,
    ] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }

    // Fail fast if specification.md is empty
    match fs::read_to_string(SPECIFICATION_PATH) {
        Ok(content) if content.trim().is_empty() => {
            log_error(&format!(
                "{} is empty — fill it in before running",
                SPECIFICATION_PATH
            ));
            process::exit(1);
        }
        Err(e) => {
            log_error(&format!("cannot read {}: {}", SPECIFICATION_PATH, e));
            process::exit(1);
        }
        _ => {}
    }

    // Ensure working files exist
    for path in &[
        SAGA_NOTES_PATH,
        DECISIONS_PATH,
        SUB_PLAN_PATH,
        NOTES_PATH,
        VERDICT_PATH,
        GUARD_RESULTS_PATH,
    ] {
        if !Path::new(path).exists() {
            if let Err(e) = fs::write(path, "") {
                log_error(&format!("cannot create {}: {}", path, e));
                process::exit(1);
            }
        }
    }

    log("Preflight OK");

    // Protected files for the saga runner
    let saga_protected: Vec<&str> = vec![
        SAGA_PROTOCOL_PATH,
        PROTOCOL_PATH,
        JUDGE_PATH,
        CONF_PATH,
    ];

    let backup_dir = backup_files(&saga_protected);
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut cycle: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return 130;
        }
        cycle += 1;
        eprintln!();
        eprintln!(
            "{}{}╔══════════════════════════════════════╗{}",
            BOLD, ORANGE, RESET
        );
        eprintln!(
            "{}{}║       Saga Cycle {:>4}                ║{}",
            BOLD, ORANGE, cycle, RESET
        );
        eprintln!(
            "{}{}╚══════════════════════════════════════╝{}",
            BOLD, ORANGE, RESET
        );

        // Restore protected files
        restore_files(&runner.backup_dir, &saga_protected);

        if dry_run {
            log("(dry-run) Skipping scoper invocation");
        } else {
            // Invoke Agent 1 (scoper)
            eprintln!();
            eprintln!(
                "{}{}┌\u{2500} Scoper \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}┐{}",
                BOLD, BLUE, RESET
            );
            eprintln!(
                "{}{}│  Invoking scoper (cycle {:>4})         │{}",
                BOLD, BLUE, cycle, RESET
            );
            eprintln!(
                "{}{}└\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}┘{}",
                BOLD, BLUE, RESET
            );

            let scoper_ok = invoke_scoper(&mut runner, &config, cycle);
            if !scoper_ok {
                log_error("Scoper invocation failed \u{2014} aborting saga");
                return 1;
            }

            if signal::interrupted() {
                log("Interrupted \u{2014} shutting down");
                return 130;
            }
        }

        // Check if scoper signaled DONE
        if is_saga_done() {
            eprintln!();
            eprintln!(
                "{}{}  Scoper signals DONE \u{2014} saga complete  {}",
                GREEN, BOLD, RESET
            );
            eprintln!();
            return 0;
        }

        // Verify sub-plan.md is non-empty
        match fs::read_to_string(SUB_PLAN_PATH) {
            Ok(content) if content.trim().is_empty() => {
                log_error("Scoper produced an empty sub-plan.md \u{2014} aborting saga");
                return 1;
            }
            Err(e) => {
                log_error(&format!("cannot read {}: {}", SUB_PLAN_PATH, e));
                return 1;
            }
            _ => {}
        }

        if dry_run {
            log("(dry-run) Skipping brute loop on sub-plan.md");
            log("(dry-run) Exiting after one cycle");
            return 0;
        }

        // Run the brute loop on sub-plan.md
        log("Running brute loop on sub-plan.md...");

        // Clear notes.md, verdict.md, and guard-results.md for the inner brute cycle
        let _ = fs::write(NOTES_PATH, "");
        let _ = fs::write(VERDICT_PATH, "");
        let _ = fs::write(GUARD_RESULTS_PATH, "");

        let brute_result = run_brute_core(&config, SUB_PLAN_PATH, false);

        match brute_result {
            BruteResult::Pass => {
                log(&format!(
                    "{}Sub-plan passed \u{2014} looping back to scoper{}",
                    GREEN, RESET
                ));
            }
            BruteResult::Bailout => {
                log(&format!(
                    "{}Brute loop bailed out \u{2014} looping back to scoper for re-scoping{}",
                    ORANGE, RESET
                ));
                // verdict.md and notes.md are preserved — scoper will read them
            }
            BruteResult::Interrupt => {
                log("Interrupted \u{2014} shutting down");
                return 130;
            }
            BruteResult::Error => {
                log_error("Brute loop encountered a hard error \u{2014} aborting saga");
                return 1;
            }
        }
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
            let mode = match args.get(2).map(|a| a.as_str()) {
                None => "loop",
                Some("brute") => "brute",
                Some("saga") => "saga",
                Some(other) => {
                    log_error(&format!("unknown argument '{}'", other));
                    print_init_help();
                    process::exit(2);
                }
            };
            if args.len() > 3 {
                log_error(&format!("unexpected argument '{}'", args[3]));
                print_init_help();
                process::exit(2);
            }
            process::exit(init(mode));
        }
        "clean" => {
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_clean_help();
                return;
            }
            if args.len() > 2 {
                log_error(&format!("unexpected argument '{}'", args[2]));
                print_clean_help();
                process::exit(2);
            }
            process::exit(clean());
        }
        "stash" => {
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_stash_help();
                return;
            }
            match args.get(2).map(|a| a.as_str()) {
                None => {
                    process::exit(stash_create());
                }
                Some("log") => {
                    process::exit(stash_log_cmd());
                }
                Some("checkout") => match args.get(3) {
                    Some(hash) => process::exit(stash_checkout(hash)),
                    None => {
                        log_error("missing hash — usage: yoke stash checkout <hash>");
                        process::exit(2);
                    }
                },
                Some("pop") => {
                    process::exit(stash_pop());
                }
                Some(other) => {
                    log_error(&format!("unknown subcommand '{}'", other));
                    print_stash_help();
                    process::exit(2);
                }
            }
        }
        "layer" => {
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_layer_help();
                return;
            }
            if args.get(2).is_some_and(|a| a == "--list") {
                for name in list_layers() {
                    println!("  {}", name);
                }
                return;
            }
            let names_arg = match args.get(2) {
                Some(a) => a.clone(),
                None => {
                    log_error("missing layer name");
                    print_layer_help();
                    process::exit(2);
                }
            };
            process::exit(apply_layers(&names_arg));
        }
        "run" => {
            // Check for --help before other flags
            if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
                print_run_help();
                return;
            }
            let mut dry_run = false;
            let mut no_sandbox = false;
            for arg in &args[2..] {
                match arg.as_str() {
                    "--dry-run" => dry_run = true,
                    "--no-sandbox" => no_sandbox = true,
                    other => {
                        log_error(&format!("unknown flag '{}'", other));
                        print_run_help();
                        process::exit(2);
                    }
                }
            }
            // Load config to determine runner type
            let config = match Config::load(Path::new(CONF_PATH)) {
                Ok(c) => c,
                Err(e) => {
                    log_error(&e);
                    process::exit(1);
                }
            };
            if config.image.is_none() && !no_sandbox {
                log_error("no 'image' directive in config — refusing to run without sandbox");
                eprintln!("  Add 'image <name>' to {} or pass --no-sandbox to override.", CONF_PATH);
                process::exit(2);
            }
            match detect_mode() {
                Some("saga") => {
                    log("Detected saga mode");
                    process::exit(run_saga(dry_run))
                }
                Some("brute") => {
                    process::exit(run_brute(dry_run))
                }
                _ => {
                    process::exit(run_loop(dry_run))
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
