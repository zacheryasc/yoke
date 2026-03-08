#![warn(clippy::string_slice)]

mod ansi;
mod boundary;
mod config;
mod guard;
mod json;
mod signal;
mod stash;
mod stream;
mod stream_opencode;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};

use config::{Backend, Config, Periodic};

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
const SAGA_LOG_PATH: &str = ".loop/saga-log.md";
const DECISIONS_PATH: &str = ".loop/decisions.md";
const SUB_PLAN_PATH: &str = ".loop/sub-plan.md";
pub(crate) const STASH_DIR: &str = ".loop/.stash";

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

use ansi::{BLUE, BOLD, DIM, GREEN, ORANGE, RED, RESET};
const MAGENTA: &str = "\x1b[38;5;207m";

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

pub(crate) fn log(msg: &str) {
    eprintln!("{}{}[yoke]{} {}", ORANGE, BOLD, RESET, msg);
}

pub(crate) fn log_error(msg: &str) {
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
    eprintln!("  Switching modes stashes the current .loop/ state and creates fresh");
    eprintln!("  target mode files. Restore a previous session with: yoke stash checkout");
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

/// Check the first line of a notes file for STATUS: DONE.
fn is_status_done(path: &str) -> bool {
    match fs::read_to_string(path) {
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

/// Build a docker command that runs the claude CLI inside a container.
fn build_docker_claude_command(image: &str, claude_args: &[&str]) -> Command {
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

    c.arg(image);
    c.args(claude_args);
    c
}

/// Build a Command for invoking the agent backend.
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
            match config.image {
                Some(ref image) => build_docker_claude_command(image, &claude_args),
                None => {
                    let mut c = Command::new("claude");
                    c.args(claude_args);
                    c
                }
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

/// Invoke a periodic agent with its protocol file.
/// Returns true if the invocation succeeded.
fn invoke_periodic(runner: &mut LoopRunner, config: &Config, periodic: &Periodic, iteration: u32) -> bool {
    let prompt = format!("Read {} and follow its instructions.", periodic.path);
    let (status, _) = invoke_process(
        runner, config, &prompt, &periodic.name,
        &format!("periodic-{}", periodic.name), iteration, 0.0,
    );
    status
}

/// Render a box-drawn iteration banner to stderr.
/// Uses double borders (╔/║/╚) at top level, single borders (┌/│/└) when nested.
fn render_iteration_banner(label: &str, iteration: u32, nested: bool) {
    let content = format!("{} {:>4}", label, iteration);
    // Pad content to fill inner width of 38 characters
    let inner = format!("{:^38}", content);
    if nested {
        eprintln!("{}{}┌──────────────────────────────────────┐{}", BOLD, ORANGE, RESET);
        eprintln!("{}{}│{}│{}", BOLD, ORANGE, inner, RESET);
        eprintln!("{}{}└──────────────────────────────────────┘{}", BOLD, ORANGE, RESET);
    } else {
        eprintln!("{}{}╔══════════════════════════════════════╗{}", BOLD, ORANGE, RESET);
        eprintln!("{}{}║{}║{}", BOLD, ORANGE, inner, RESET);
        eprintln!("{}{}╚══════════════════════════════════════╝{}", BOLD, ORANGE, RESET);
    }
}

/// Render a named section banner to stderr (for judge, periodic, etc.).
fn render_section_banner(title: &str, subtitle: &str, color: &str) {
    let fill_len = 40usize.saturating_sub(title.len() + 4); // "┌─ Title ──...──┐"
    eprintln!("{}{}┌─ {} {}┐{}", BOLD, color, title, "─".repeat(fill_len), RESET);
    let inner = format!("  {:<38}", subtitle);
    eprintln!("{}{}│{}│{}", BOLD, color, inner, RESET);
    eprintln!("{}{}└────────────────────────────────────────┘{}", BOLD, color, RESET);
}

/// Render a box-drawn table of guard results to stderr.
fn render_guard_table(results: &[guard::GuardResult]) {
    if results.is_empty() {
        return;
    }
    let cmd_w = results.iter().map(|r| r.name.len()).max().unwrap_or(10).max(10);
    let stat_w = 8;
    let time_w = 6;
    let top    = format!("  ┌{}┬{}┬{}┐", "─".repeat(cmd_w + 2), "─".repeat(stat_w), "─".repeat(time_w + 1));
    let bottom = format!("  └{}┴{}┴{}┘", "─".repeat(cmd_w + 2), "─".repeat(stat_w), "─".repeat(time_w + 1));
    eprintln!("{}", top);
    for r in results {
        let cmd_padded = format!("{:<w$}", r.name, w = cmd_w);
        let status_cell = if r.skipped {
            format!("{}SKIPPED {}", ORANGE, RESET)
        } else if r.passed {
            format!(" {} PASS {}  ", GREEN, RESET)
        } else {
            format!(" {} FAIL {}  ", RED, RESET)
        };
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

/// Run guard-after commands for a periodic agent.
/// Results are written to a separate file so the worker isn't confused.
fn run_periodic_guards(periodic: &Periodic, max_tail: usize) {
    if periodic.guards.is_empty() {
        return;
    }
    let results_path = PathBuf::from(format!(".loop/periodic-{}-results.md", periodic.name));
    let results = guard::run_guards(&periodic.guards, max_tail, &results_path);

    render_guard_table(&results);

    let all_passed = results.iter().all(|r| r.passed);
    if !all_passed {
        log(&format!(
            "{}WARNING: periodic '{}' guard(s) failed — results in {}{}",
            ORANGE, periodic.name, format!(".loop/periodic-{}-results.md", periodic.name), RESET
        ));
    }
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

    render_guard_table(&guard_results);

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
    let mode = detect_mode().unwrap_or("unknown");
    let stashed = stash::stash_working_files(mode);

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
            (SAGA_LOG_PATH, ""),
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

/// Stash current mode's files, clear `.loop/`, and fresh-init the target mode.
fn switch_mode(current: &str, target: &str) -> i32 {
    // 1. Stash current state
    match stash::stash_snapshot(current) {
        Ok(hash) => {
            log(&format!(
                "stashed {} state \u{2192} {}{}{}",
                current, BLUE, hash, RESET
            ));
        }
        Err(msg) => {
            log_error(&format!("failed to stash current state: {}", msg));
            return 1;
        }
    }

    // 2. Remove all non-dot files from .loop/
    let current_files = stash::collect_stashable_files();
    for (name, _) in &current_files {
        let _ = fs::remove_file(Path::new(".loop").join(name));
    }

    // 3. Fresh-init target mode
    let target_files = mode_files(target).unwrap();
    for (path, content) in &target_files {
        if let Err(e) = fs::write(path, content) {
            log_error(&format!("failed to write {}: {}", path, e));
            return 1;
        }
        log(&format!("created: {}", path));
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

/// Outcome of a plan loop run.
enum PlanLoopOutcome {
    /// STATUS: DONE + guards pass (no embedded judge).
    Done,
    /// STATUS: DONE + guards pass + embedded judge PASS (judge-every mode).
    JudgePass,
    /// Consecutive judge failures hit max-judge-failures.
    JudgeBailout,
    /// User interrupted (SIGINT).
    Interrupt,
    /// Hard error (spawn failure, missing files, etc.).
    Error,
}

/// Check if consecutive judge failures have hit the bail threshold.
/// Returns true if the loop should bail out.
fn is_judge_bailout(consecutive: u32, max: u32) -> bool {
    if consecutive >= max {
        eprintln!();
        eprintln!(
            "{}{}  {} consecutive judge FAILs \u{2014} bailing out  {}",
            RED, BOLD, max, RESET
        );
        eprintln!();
        true
    } else {
        false
    }
}

/// Result of evaluating judge-every logic within a plan loop iteration.
enum JudgeEveryAction {
    /// Judge passed on DONE — exit loop.
    Pass,
    /// Consecutive failures hit threshold — bail out.
    Bailout,
    /// Continue to next iteration (judge failed on DONE, or mid-loop check done).
    Continue,
}

/// Evaluate judge-every logic: fire judge when worker signals DONE or at cadence checkpoints.
fn evaluate_judge_every(
    runner: &mut LoopRunner,
    config: &Config,
    iteration: u32,
    guards_passed: bool,
    consecutive_judge_failures: &mut u32,
    judge_every: u32,
) -> JudgeEveryAction {
    if guards_passed && is_status_done(NOTES_PATH) {
        // Always fire judge when worker signals DONE
        eprintln!();
        render_section_banner("Judge (DONE)", "Worker DONE — invoking judge", BLUE);
        let pass = invoke_judge(runner, config, iteration);
        if pass {
            return JudgeEveryAction::Pass;
        }
        *consecutive_judge_failures += 1;
        if is_judge_bailout(*consecutive_judge_failures, config.max_judge_failures) {
            return JudgeEveryAction::Bailout;
        }
        log(&format!(
            "{}Judge FAIL ({}/{}) \u{2014} resetting STATUS for retry{}",
            ORANGE, *consecutive_judge_failures, config.max_judge_failures, RESET
        ));
        reset_notes_status();
    } else if guards_passed && iteration % judge_every == 0 {
        // Mid-loop quality checkpoint
        eprintln!();
        render_section_banner(
            "Mid-loop Judge",
            &format!("Quality checkpoint (iteration {:>4})", iteration),
            BLUE,
        );
        let pass = invoke_judge(runner, config, iteration);
        if pass {
            *consecutive_judge_failures = 0;
        } else {
            *consecutive_judge_failures += 1;
            if is_judge_bailout(*consecutive_judge_failures, config.max_judge_failures) {
                return JudgeEveryAction::Bailout;
            }
        }
        // Either way, worker continues — verdict.md has feedback
    }
    JudgeEveryAction::Continue
}

/// Validate config preconditions for the plan loop. Exits on failure.
fn validate_loop_config(config: &Config) {
    if config.judge_every.is_some() && !Path::new(JUDGE_PATH).exists() {
        log_error("judge-every is set but .loop/judge.md not found");
        process::exit(1);
    }

    for periodic in &config.periodics {
        let p = Path::new(&periodic.path);
        if !p.exists() {
            log_error(&format!(
                "periodic '{}' references missing file: {}",
                periodic.name, periodic.path
            ));
            process::exit(1);
        }
        match fs::read_to_string(p) {
            Ok(content) if content.trim().is_empty() => {
                log_error(&format!(
                    "periodic '{}' protocol file is empty: {}",
                    periodic.name, periodic.path
                ));
                process::exit(1);
            }
            _ => {}
        }
    }
}

/// Fire any periodic agents whose cadence matches this iteration.
fn fire_periodic_agents(
    runner: &mut LoopRunner,
    config: &Config,
    iteration: u32,
) {
    for periodic in &config.periodics {
        if iteration % periodic.cadence == 0 {
            eprintln!();
            let upper_name = capitalize_first(&periodic.name);
            render_section_banner(
                &upper_name,
                &format!("Periodic agent (iteration {:>4})", iteration),
                MAGENTA,
            );
            let ok = invoke_periodic(runner, config, periodic, iteration);
            if !ok {
                log(&format!(
                    "{}WARNING: periodic '{}' invocation failed — continuing{}",
                    ORANGE, periodic.name, RESET
                ));
            }
            run_periodic_guards(periodic, config.max_tail);
        }
    }
}

/// Result of running the worker + guards for a single plan-loop iteration.
enum IterationStepResult {
    /// Worker + guards succeeded, loop should continue.
    Continue { guards_passed: bool, cost: f64 },
    /// Terminal outcome — return immediately from the loop.
    Terminal(PlanLoopOutcome),
}

/// Run the worker invocation, interrupt check, and guard evaluation for one iteration.
/// Returns `Continue` with guard results when the loop should proceed, or `Terminal`
/// when an early exit is needed.
fn run_iteration_step(
    runner: &mut LoopRunner,
    config: &Config,
    iteration: u32,
    prior_total: f64,
    dry_run: bool,
) -> IterationStepResult {
    if dry_run {
        log(&format!(
            "(dry-run) Skipping {} invocation",
            match config.backend() {
                Backend::Claude => "Claude",
                Backend::OpenCode => "OpenCode",
            }
        ));
    } else {
        let (success, iter_cost) = invoke_agent(runner, config, iteration, prior_total);
        if !success {
            log_error("Claude invocation failed — aborting loop");
            return IterationStepResult::Terminal(PlanLoopOutcome::Error);
        }
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return IterationStepResult::Terminal(PlanLoopOutcome::Interrupt);
        }
        let guards_passed = run_all_guards(config);
        if guards_passed {
            log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));
        } else {
            log(&format!(
                "{}Some guards failed \u{2014} Claude will see results next iteration{}",
                ORANGE, RESET
            ));
        }
        return IterationStepResult::Continue { guards_passed, cost: iter_cost };
    }

    // Dry-run path: run guards and exit after one iteration.
    let guards_passed = run_all_guards(config);
    if guards_passed {
        log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));
        log("(dry-run) Guards passed \u{2014} exiting after one iteration");
        IterationStepResult::Terminal(PlanLoopOutcome::Done)
    } else {
        log(&format!(
            "{}Some guards failed{}", ORANGE, RESET
        ));
        log("(dry-run) Exiting after one iteration");
        IterationStepResult::Terminal(PlanLoopOutcome::Error)
    }
}

/// Core plan-loop runner that can be called standalone or nested inside brute.
///
/// - `config`: already-loaded Config
/// - `plan_path`: path to the plan file (e.g. PLAN_PATH or SUB_PLAN_PATH)
/// - `dry_run`: if true, skip Claude invocation (one iteration only)
/// - `nested`: if true, running inside brute (adjusts output banners)
fn run_plan_loop(config: &Config, plan_path: &str, dry_run: bool, nested: bool) -> PlanLoopOutcome {
    preflight(plan_path, !nested);
    validate_loop_config(config);
    log("Preflight OK");

    let mut protected: Vec<&str> = vec![PROTOCOL_PATH, plan_path, CONF_PATH];
    let periodic_paths: Vec<String> = config.periodics.iter().map(|p| p.path.clone()).collect();
    for pp in &periodic_paths {
        protected.push(pp.as_str());
    }

    let backup_dir = backup_files(&protected);
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    let mut iteration: u32 = 0;
    let mut total_cost: f64 = 0.0;
    let mut consecutive_judge_failures: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return PlanLoopOutcome::Interrupt;
        }
        iteration += 1;
        eprintln!();
        let label = if nested { "Plan Iteration" } else { "Iteration" };
        render_iteration_banner(label, iteration, nested);

        if let Some((completed, total)) = stage_progress(plan_path) {
            eprintln!("{}", format_progress_bar(completed, total));
        }

        restore_files(&runner.backup_dir, &protected);

        let guards_passed = match run_iteration_step(&mut runner, config, iteration, total_cost, dry_run) {
            IterationStepResult::Continue { guards_passed, cost } => {
                total_cost += cost;
                guards_passed
            }
            IterationStepResult::Terminal(outcome) => return outcome,
        };

        fire_periodic_agents(&mut runner, config, iteration);

        if let Some(judge_every) = config.judge_every {
            match evaluate_judge_every(
                &mut runner, config, iteration, guards_passed,
                &mut consecutive_judge_failures, judge_every,
            ) {
                JudgeEveryAction::Pass => return PlanLoopOutcome::JudgePass,
                JudgeEveryAction::Bailout => return PlanLoopOutcome::JudgeBailout,
                JudgeEveryAction::Continue => continue,
            }
        }

        if guards_passed && is_status_done(NOTES_PATH) {
            eprintln!();
            eprintln!(
                "{}{}  STATUS: DONE and all guards pass \u{2014} loop complete  {}",
                GREEN, BOLD, RESET
            );
            eprintln!();
            return PlanLoopOutcome::Done;
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
        PlanLoopOutcome::Done | PlanLoopOutcome::JudgePass => 0,
        PlanLoopOutcome::JudgeBailout | PlanLoopOutcome::Error => 1,
        PlanLoopOutcome::Interrupt => 130,
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
        render_iteration_banner("Brute Iteration", iteration, false);

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
                PlanLoopOutcome::JudgePass => {
                    // Embedded judge already passed — skip standalone judge
                    log("Plan runner completed with embedded judge PASS");
                    return BruteResult::Pass;
                }
                PlanLoopOutcome::Done => {
                    // No embedded judge — fall through to standalone judge
                    log("Plan runner completed successfully");
                }
                PlanLoopOutcome::JudgeBailout => {
                    return BruteResult::Bailout;
                }
                PlanLoopOutcome::Interrupt => {
                    log("Interrupted \u{2014} shutting down");
                    return BruteResult::Interrupt;
                }
                PlanLoopOutcome::Error => {
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
        render_section_banner(
            "Judge",
            &format!("Invoking judge (attempt {:>4})", iteration),
            BLUE,
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
        let max_failures = config.max_judge_failures;
        log(&format!(
            "{}Judge says FAIL ({}/{}) \u{2014} {}{}",
            ORANGE,
            consecutive_failures,
            max_failures,
            if consecutive_failures >= max_failures {
                "bailing out"
            } else {
                "worker will see verdict next iteration"
            },
            RESET
        ));

        if is_judge_bailout(consecutive_failures, max_failures) {
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
/// Preflight checks for saga mode: verify required files exist, critical files
/// are non-empty, and create working files if missing.
fn saga_preflight() {
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
}

/// Run one saga cycle: invoke the scoper, check for DONE, then run brute on the sub-plan.
/// Returns Some(exit_code) if the saga should terminate, None to continue looping.
fn run_saga_cycle(
    runner: &mut LoopRunner,
    config: &Config,
    cycle: u32,
    dry_run: bool,
    saga_protected: &[&str],
) -> Option<i32> {
    restore_files(&runner.backup_dir, saga_protected);

    if dry_run {
        log("(dry-run) Skipping scoper invocation");
    } else {
        eprintln!();
        render_section_banner(
            "Scoper",
            &format!("Invoking scoper (cycle {:>4})", cycle),
            BLUE,
        );

        if !invoke_scoper(runner, config, cycle) {
            log_error("Scoper invocation failed \u{2014} aborting saga");
            return Some(1);
        }

        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return Some(130);
        }
    }

    if is_status_done(SAGA_NOTES_PATH) {
        eprintln!();
        eprintln!(
            "{}{}  Scoper signals DONE \u{2014} saga complete  {}",
            GREEN, BOLD, RESET
        );
        eprintln!();
        return Some(0);
    }

    match fs::read_to_string(SUB_PLAN_PATH) {
        Ok(content) if content.trim().is_empty() => {
            log_error("Scoper produced an empty sub-plan.md \u{2014} aborting saga");
            return Some(1);
        }
        Err(e) => {
            log_error(&format!("cannot read {}: {}", SUB_PLAN_PATH, e));
            return Some(1);
        }
        _ => {}
    }

    if dry_run {
        log("(dry-run) Skipping brute loop on sub-plan.md");
        log("(dry-run) Exiting after one cycle");
        return Some(0);
    }

    log("Running brute loop on sub-plan.md...");

    // Append worker notes to saga log before clearing
    if let Ok(notes) = fs::read_to_string(NOTES_PATH) {
        if !notes.trim().is_empty() {
            let header = format!("\n## Chunk {}\n\n", cycle);
            let _ = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(SAGA_LOG_PATH)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(header.as_bytes())?;
                    f.write_all(notes.as_bytes())?;
                    if !notes.ends_with('\n') {
                        f.write_all(b"\n")?;
                    }
                    Ok(())
                });
        }
    }

    let _ = fs::write(NOTES_PATH, "");
    let _ = fs::write(VERDICT_PATH, "");
    let _ = fs::write(GUARD_RESULTS_PATH, "");

    match run_brute_core(config, SUB_PLAN_PATH, false) {
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
        }
        BruteResult::Interrupt => {
            log("Interrupted \u{2014} shutting down");
            return Some(130);
        }
        BruteResult::Error => {
            log_error("Brute loop encountered a hard error \u{2014} aborting saga");
            return Some(1);
        }
    }
    None
}

fn run_saga(dry_run: bool) -> i32 {
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

    saga_preflight();

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
        render_iteration_banner("Saga Cycle", cycle, false);

        if let Some(exit_code) = run_saga_cycle(&mut runner, &config, cycle, dry_run, &saga_protected) {
            return exit_code;
        }
    }
}

fn cmd_init(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        print_init_help();
        process::exit(0);
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

fn cmd_clean(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        print_clean_help();
        process::exit(0);
    }
    if args.len() > 2 {
        log_error(&format!("unexpected argument '{}'", args[2]));
        print_clean_help();
        process::exit(2);
    }
    process::exit(clean());
}

fn cmd_stash(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        stash::print_stash_help();
        process::exit(0);
    }
    let mode = detect_mode().unwrap_or("unknown");
    match args.get(2).map(|a| a.as_str()) {
        None => {
            process::exit(stash::stash_create(mode));
        }
        Some("log") => {
            process::exit(stash::stash_log_cmd());
        }
        Some("checkout") => match args.get(3) {
            Some(hash) => process::exit(stash::stash_checkout(hash, mode)),
            None => {
                log_error("missing hash — usage: yoke stash checkout <hash>");
                process::exit(2);
            }
        },
        Some("pop") => {
            process::exit(stash::stash_pop(mode));
        }
        Some(other) => {
            log_error(&format!("unknown subcommand '{}'", other));
            stash::print_stash_help();
            process::exit(2);
        }
    }
}

fn cmd_layer(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        print_layer_help();
        process::exit(0);
    }
    if args.get(2).is_some_and(|a| a == "--list") {
        for name in list_layers() {
            println!("  {}", name);
        }
        process::exit(0);
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

fn cmd_run(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        print_run_help();
        process::exit(0);
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

fn main() {
    signal::install();
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    match args[1].as_str() {
        "init" => cmd_init(&args),
        "clean" => cmd_clean(&args),
        "stash" => cmd_stash(&args),
        "layer" => cmd_layer(&args),
        "run" => cmd_run(&args),
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
