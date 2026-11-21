#![warn(clippy::string_slice)]

mod ansi;
mod artifacts;
mod boundary;
mod config;
mod guard;
mod json;
mod metrics;
mod runtime;
mod signal;
mod stash;
mod stream;
mod stream_omp;
mod summary;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};

use config::{Config, Periodic};

const CONF_PATH: &str = ".loop/yoke.conf";
const NOTES_PATH: &str = ".loop/notes.md";
const ARCHIVE_DIR: &str = ".loop/archive";
const GUARD_RESULTS_PATH: &str = ".loop/guard-results.md";
const PROTOCOL_PATH: &str = ".loop/protocol.md";
const PLAN_PATH: &str = ".loop/plan.md";
const JUDGE_PATH: &str = ".loop/judge.md";
const VERDICT_PATH: &str = ".loop/verdict.md";

const DEFAULT_JUDGE: &str = include_str!("templates/brute/judge.md");

const BRIEFING_PATH: &str = ".loop/briefing.md";

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
    eprintln!(
        "{}{}[yoke]{} {}{}ERROR:{} {}",
        ORANGE, BOLD, RESET, BOLD, RED, RESET, msg
    );
}

fn get_layer(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "repl" => Some(("REPL probing", include_str!("templates/layers/repl.md"))),
        _ => None,
    }
}

/// Print usage or per-command help.
fn show_help(topic: &str) {
    if topic == "inject" {
        eprint!(
            "[yoke inject] steer an active run\n\n\
             USAGE: yoke inject [--shutdown] <message>\n\n\
             OPTIONS:\n  --shutdown      Ask the worker to preserve progress and exit promptly\n  --help, -h      Show this help message\n\n\
             Sends the message to the active run through {}.\n",
            runtime::YOKE_SOCKET_PATH
        );
        return;
    }
    eprint!(
        "{}",
        match topic {
            "init" => include_str!("help/init.txt"),
            "clean" => include_str!("help/clean.txt"),
            "layer" => include_str!("help/layer.txt"),
            "run" => include_str!("help/run.txt"),
            "stats" => include_str!("help/stats.txt"),
            _ => include_str!("help/usage.txt"),
        }
    );
}

#[allow(clippy::string_slice)] // slices after ASCII prefixes ("## ", ". ")
fn apply_layers(names_arg: &str) -> i32 {
    let judge_path = Path::new(JUDGE_PATH);
    if !judge_path.exists() {
        log_error("no .loop/judge.md found — run 'yoke init' first");
        return 1;
    }
    let mut content = match fs::read_to_string(judge_path) {
        Ok(c) => c,
        Err(e) => {
            log_error(&format!("failed to read judge.md: {}", e));
            return 1;
        }
    };

    for name in names_arg.split(',').map(str::trim) {
        let Some((heading, layer)) = get_layer(name) else {
            log_error(&format!(
                "unknown layer '{}' — use 'yoke layer --list'",
                name
            ));
            return 2;
        };
        if content.contains(heading) {
            log(&format!("skip (already present): {}", name));
            continue;
        }
        let Some(pos) = content.find("\n## Verdict") else {
            log_error("judge.md has no '## Verdict' section — cannot insert layer");
            return 1;
        };
        content.insert_str(pos, &format!("\n{}\n", layer.trim()));
        log(&format!("applied layer: {}", name));
    }

    // Re-number "## N. section" headings sequentially.
    let mut section_num = 0;
    let mut result = String::with_capacity(content.len());
    for line in content.lines() {
        let numbered = line
            .strip_prefix("## ")
            .and_then(|rest| rest.split_once(". "))
            .filter(|(num, _)| !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()));
        match numbered {
            Some((_, heading)) => {
                section_num += 1;
                result.push_str(&format!("## {}. {}\n", section_num, heading));
            }
            None => {
                result.push_str(line);
                result.push('\n');
            }
        }
    }

    if let Err(e) = fs::write(judge_path, result.trim_end().to_string() + "\n") {
        log_error(&format!("failed to write judge.md: {}", e));
        return 1;
    }

    0
}

/// Holds loop state and cleans up on drop.
///
/// Drop kills any running OMP child process and removes the backup
/// directory. Drop can be skipped on raw SIGINT; the backup temp dir is small
/// and inconsequential.
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

fn ensure_archive_dir() {
    if let Err(e) = fs::create_dir_all(ARCHIVE_DIR) {
        log_error(&format!("cannot create {}: {}", ARCHIVE_DIR, e));
        process::exit(1);
    }
}

/// Check that required loop files exist, create notes if missing.
///
/// The previous attempt's guard feedback is preserved so the worker can
/// read it on the next iteration.

/// Copy protected files into an existing backup directory. Used both at run
/// start and when a paused run resumes, so paused edits become the new
/// restore point instead of being clobbered by the next iteration.
pub(crate) fn snapshot_files_to<S: AsRef<str>>(backup_dir: &Path, files: &[S]) -> bool {
    let mut ok = true;
    for path in files {
        let path = path.as_ref();
        let src = Path::new(path);
        if src.exists() {
            let dest = backup_dir.join(src.file_name().unwrap());
            if let Err(e) = fs::copy(src, &dest) {
                log_error(&format!("failed to snapshot {}: {}", path, e));
                ok = false;
            }
        }
    }
    ok
}

/// Copy the given files to a backup directory inside .loop/. Returns the backup path.
fn backup_files(files: &[&str]) -> PathBuf {
    let backup_dir = PathBuf::from(format!(".loop/backups-{}", process::id()));
    if let Err(e) = fs::create_dir_all(&backup_dir) {
        log_error(&format!("failed to create backup dir: {}", e));
        process::exit(1);
    }
    if !snapshot_files_to(&backup_dir, files) {
        process::exit(1);
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

/// Detect the home directory of the default user inside a Docker image.
fn container_home(image: &str) -> String {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "echo $HOME",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let h = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if h.is_empty() {
                "/home/sandbox".to_string()
            } else {
                h
            }
        }
        _ => "/home/sandbox".to_string(),
    }
}

fn should_forward_env(key: &str) -> bool {
    key.starts_with("OMP_")
        || key.starts_with("PI_")
        || key.starts_with("ANTHROPIC_")
        || key.starts_with("OPENAI_")
        || key.starts_with("GEMINI_")
        || key.starts_with("OPENROUTER_")
        || key.starts_with("CLAUDE_CODE_")
        || key.starts_with("FOUNDRY_")
        || key == "NODE_EXTRA_CA_CERTS"
        || key.ends_with("_API_KEY")
        || key.ends_with("_OAUTH_TOKEN")
        || key.ends_with("_ACCESS_TOKEN")
}

/// Build a docker command that runs OMP inside a container.
pub(crate) fn build_docker_omp_command(image: &str, omp_args: &[String]) -> Command {
    let workdir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .to_string();

    let container_home = container_home(image);

    let mut c = Command::new("docker");
    c.args([
        "run",
        "-i",
        "--rm",
        "--network=host",
        "--cap-add=NET_ADMIN",
        "--cap-add=NET_RAW",
    ]);
    c.args(["--entrypoint", "omp"]);
    c.arg("-v").arg(format!("{}:/workspace", workdir));
    c.arg("-w").arg("/workspace");

    // Mount Docker socket for Docker-outside-of-Docker
    let docker_sock = "/var/run/docker.sock";
    if std::path::Path::new(docker_sock).exists() {
        c.arg("-v").arg(format!("{0}:{0}", docker_sock));
        if let Ok(meta) = std::fs::metadata(docker_sock) {
            use std::os::unix::fs::MetadataExt;
            c.arg("--group-add").arg(meta.gid().to_string());
        }
    }

    for (key, val) in std::env::vars() {
        if should_forward_env(&key) {
            c.arg("-e").arg(format!("{}={}", key, val));
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let home_path = PathBuf::from(&home);
        let omp_dir = home_path.join(".omp");
        if omp_dir.exists() {
            c.arg("-v")
                .arg(format!("{}:{}/.omp", omp_dir.display(), container_home));
        }
    }

    c.arg(image);
    c.args(omp_args);
    c
}

/// Build the OMP option list shared by print/json and RPC modes.
pub(crate) fn omp_args(config: &Config, mode: &str) -> Vec<String> {
    let mut args = vec![
        "--mode".to_string(),
        mode.to_string(),
        "--auto-approve".to_string(),
    ];
    if let Some(model) = config.model.as_deref() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(thinking) = config.thinking {
        args.push("--thinking".to_string());
        args.push(thinking.as_omp_arg().to_string());
    }
    args
}

/// Build a Command for invoking OMP, in a container when configured.
pub(crate) fn omp_command(config: &Config, omp_args: &[String]) -> Command {
    match &config.image {
        Some(image) => build_docker_omp_command(image, omp_args),
        None => {
            let mut c = Command::new("omp");
            c.args(omp_args);
            c
        }
    }
}

/// Build a print-mode Command for invoking OMP.
///
/// `resume` carries an OMP session id when the harness wants to continue the
/// previous round's conversation. `None` produces a fresh session.
fn build_command(config: &Config, prompt: &str, resume: Option<&str>) -> Command {
    let mut args = omp_args(config, "json");
    if let Some(sid) = resume {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    args.push("-p".to_string());
    args.push(prompt.to_string());
    omp_command(config, &args)
}

/// Wall-clock + stream summary for one agent invocation.
pub(crate) struct AgentRunStats {
    pub success: bool,
    pub wall_secs: f64,
    pub stream: stream::StreamSummary,
}

/// Spawn an agent process, stream its output, kill it when streaming ends,
/// and wait for exit.  Returns an `AgentRunStats` carrying success, the
/// yoke-measured wall clock around the whole spawn+stream+wait, and the
/// stream-derived summary (cost, thinking, per-tool durations).
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
    resume: Option<&str>,
) -> AgentRunStats {
    let in_container = config.image.is_some();
    let wall_start = std::time::Instant::now();

    log(&format!(
        "Launching {} {}(iteration {}){}...",
        label,
        if in_container { "in container " } else { "" },
        iteration,
        match resume {
            Some(sid) => format!(" [resume {}…]", sid.chars().take(8).collect::<String>()),
            None => String::new(),
        }
    ));

    let mut cmd = build_command(config, prompt, resume);

    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let bin = if config.image.is_some() {
                "docker"
            } else {
                "omp"
            };
            log_error(&format!("failed to spawn '{}': {}", bin, e));
            return AgentRunStats {
                success: false,
                wall_secs: wall_start.elapsed().as_secs_f64(),
                stream: stream::StreamSummary::default(),
            };
        }
    };

    signal::set_child_pid(child.id() as i32);

    let log_path = config
        .log_dir
        .as_ref()
        .map(|dir| PathBuf::from(dir).join(format!("{}-{}.jsonl", log_prefix, iteration)));

    let mut summary = stream::StreamSummary::default();
    if let Some(stdout) = child.stdout.take() {
        runner.child = Some(child);
        summary = stream_omp::filter_stream(stdout, log_path.as_deref(), prior_total);
        // Kill the child after streaming ends — prevents deadlock if it
        // ignores SIGPIPE and keeps writing.  No-op if already exited.
        if let Some(ch) = &mut runner.child {
            let _ = ch.kill();
        }
        if signal::interrupted() {
            return AgentRunStats {
                success: false,
                wall_secs: wall_start.elapsed().as_secs_f64(),
                stream: summary,
            };
        }
    } else {
        runner.child = Some(child);
    }

    let status = if let Some(child) = &mut runner.child {
        match child.wait() {
            Ok(s) => {
                if signal::interrupted() {
                    return AgentRunStats {
                        success: false,
                        wall_secs: wall_start.elapsed().as_secs_f64(),
                        stream: summary,
                    };
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
    AgentRunStats {
        success: status,
        wall_secs: wall_start.elapsed().as_secs_f64(),
        stream: summary,
    }
}

/// Invoke the agent, piping stdout through the stream filter.
fn invoke_agent(
    supervisor: &mut runtime::RunSupervisor,
    config: &Config,
    iteration: u32,
    prior_total: f64,
) -> AgentRunStats {
    let prompt = "Read .loop/protocol.md and follow its instructions.";
    supervisor.invoke_worker(config, prompt, "OMP", "iteration", iteration, prior_total)
}

/// Invoke a periodic agent with its protocol file.
/// Returns true if the invocation succeeded.
///
/// Periodic agents always run in a fresh session: they have a different
/// protocol than the main worker.
fn invoke_periodic(
    runner: &mut LoopRunner,
    config: &Config,
    periodic: &Periodic,
    iteration: u32,
) -> bool {
    let prompt = format!("Read {} and follow its instructions.", periodic.path);
    let stats = invoke_process(
        runner,
        config,
        &prompt,
        &periodic.name,
        &format!("periodic-{}", periodic.name),
        iteration,
        0.0,
        None,
    );
    stats.success
}

/// Render a box-drawn iteration banner to stderr.
/// Uses double borders (╔/║/╚) at top level, single borders (┌/│/└) when nested.
fn render_iteration_banner(label: &str, iteration: u32, nested: bool) {
    let content = format!("{} {:>4}", label, iteration);
    // Pad content to fill inner width of 38 characters
    let inner = format!("{:^38}", content);
    if nested {
        eprintln!(
            "{}{}┌──────────────────────────────────────┐{}",
            BOLD, ORANGE, RESET
        );
        eprintln!("{}{}│{}│{}", BOLD, ORANGE, inner, RESET);
        eprintln!(
            "{}{}└──────────────────────────────────────┘{}",
            BOLD, ORANGE, RESET
        );
    } else {
        eprintln!(
            "{}{}╔══════════════════════════════════════╗{}",
            BOLD, ORANGE, RESET
        );
        eprintln!("{}{}║{}║{}", BOLD, ORANGE, inner, RESET);
        eprintln!(
            "{}{}╚══════════════════════════════════════╝{}",
            BOLD, ORANGE, RESET
        );
    }
}

/// Render a named section banner to stderr (for judge, periodic, etc.).
fn render_section_banner(title: &str, subtitle: &str, color: &str) {
    let fill_len = 40usize.saturating_sub(title.len() + 4); // "┌─ Title ──...──┐"
    eprintln!(
        "{}{}┌─ {} {}┐{}",
        BOLD,
        color,
        title,
        "─".repeat(fill_len),
        RESET
    );
    let inner = format!("  {:<38}", subtitle);
    eprintln!("{}{}│{}│{}", BOLD, color, inner, RESET);
    eprintln!(
        "{}{}└────────────────────────────────────────┘{}",
        BOLD, color, RESET
    );
}

/// Render a box-drawn table of guard results to stderr.
fn render_guard_table(results: &[guard::GuardResult]) {
    for result in results {
        let (color, status) = if result.skipped {
            (ORANGE, "SKIPPED")
        } else if result.passed {
            (GREEN, "PASS")
        } else {
            (RED, "FAIL")
        };
        let timing = if result.skipped {
            String::new()
        } else {
            format!(" {:.1}s", result.elapsed_secs)
        };
        eprintln!(
            "  {}{:<7}{} {}{}{}",
            color, status, RESET, result.name, DIM, timing
        );
    }
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
            ORANGE,
            periodic.name,
            format!(".loop/periodic-{}-results.md", periodic.name),
            RESET
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
    let (label, color) = if pass { ("PASS", GREEN) } else { ("FAIL", RED) };
    eprintln!("\n{}{}  Judge verdict: {}  {}", BOLD, color, label, RESET);
    for line in content
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(2)
    {
        eprintln!("{}  {}{}", DIM, line, RESET);
    }
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

    // Judge is always a fresh session — independence per behavioral-specification §2.1.
    let stats = invoke_process(
        runner,
        config,
        judge_prompt,
        "judge",
        "judge",
        iteration,
        0.0,
        None,
    );
    if !stats.success {
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
    for violation in violations {
        eprintln!("  {}{}{}", RED, violation, RESET);
    }
}

/// Outcome of one boundary-check + guard-run pass.
struct GuardOutcome {
    /// True when boundary passed AND every guard passed.
    passed: bool,
    /// Per-guard results (empty when the boundary check failed and the
    /// guards were therefore skipped).
    guard_results: Vec<guard::GuardResult>,
}

/// Run boundary check and all configured guards.
fn run_all_guards(config: &Config) -> GuardOutcome {
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
        return GuardOutcome {
            passed: false,
            guard_results: Vec::new(),
        };
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

    GuardOutcome {
        passed: all_passed,
        guard_results,
    }
}

fn clean() -> i32 {
    let loop_dir = Path::new(".loop");
    if !loop_dir.exists() {
        log_error(".loop/ directory not found — nothing to clean");
        return 1;
    }

    // Stash non-empty working files before wiping
    let stashed = stash::stash_working_files("brute");

    log("Cleaning .loop/ working files...");

    // Clear working files to empty
    for path in &[PLAN_PATH, NOTES_PATH, VERDICT_PATH, GUARD_RESULTS_PATH] {
        let p = Path::new(path);
        if p.exists() {
            if let Err(e) = fs::write(p, "") {
                log_error(&format!("failed to clear {}: {}", path, e));
                return 1;
            }
            log(&format!("cleared: {}", path));
        }
    }

    let archive = Path::new(ARCHIVE_DIR);
    if archive.exists() {
        if let Err(e) = fs::remove_dir_all(archive) {
            log_error(&format!("failed to clear {}: {}", ARCHIVE_DIR, e));
            return 1;
        }
        log(&format!("cleared: {}", ARCHIVE_DIR));
    }
    ensure_archive_dir();

    // Reset judge.md to the default template.
    let judge = Path::new(JUDGE_PATH);
    if judge.exists() {
        if let Err(e) = fs::write(judge, DEFAULT_JUDGE) {
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

fn init() -> i32 {
    let loop_dir = Path::new(".loop");
    if !loop_dir.exists() {
        if let Err(e) = fs::create_dir(loop_dir) {
            log_error(&format!("failed to create .loop/: {}", e));
            return 1;
        }
        log("Created .loop/");
    }
    ensure_archive_dir();

    log("Initializing brute loop...");

    for (path, content) in [
        (CONF_PATH, include_str!("templates/brute/yoke.conf")),
        (PROTOCOL_PATH, include_str!("templates/brute/protocol.md")),
        (BRIEFING_PATH, include_str!("templates/brute/briefing.md")),
        (PLAN_PATH, ""),
        (JUDGE_PATH, DEFAULT_JUDGE),
        (NOTES_PATH, ""),
        (VERDICT_PATH, ""),
        (GUARD_RESULTS_PATH, ""),
    ] {
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
    /// Continue to the next iteration.
    Continue,
}

/// Evaluate judge-every logic: fire judge when worker signals DONE or at cadence checkpoints.
/// On firing, records wall time into `judge_secs_out` for the metrics row.
fn evaluate_judge_every(
    runner: &mut LoopRunner,
    config: &Config,
    iteration: u32,
    guards_passed: bool,
    consecutive_judge_failures: &mut u32,
    judge_every: u32,
    judge_secs_out: &mut Option<f64>,
) -> JudgeEveryAction {
    if guards_passed && is_status_done(NOTES_PATH) {
        // Always fire judge when a status-driven worker signals DONE.
        eprintln!();
        render_section_banner("Judge (DONE)", "Worker DONE — invoking judge", BLUE);
        let judge_start = std::time::Instant::now();
        let pass = invoke_judge(runner, config, iteration);
        *judge_secs_out = Some(judge_start.elapsed().as_secs_f64());
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
        return JudgeEveryAction::Continue;
    }

    if guards_passed && iteration % judge_every == 0 {
        // Cadenced quality checkpoint.
        eprintln!();
        render_section_banner(
            "Mid-loop Judge",
            &format!("Quality checkpoint (iteration {:>4})", iteration),
            BLUE,
        );
        let judge_start = std::time::Instant::now();
        let pass = invoke_judge(runner, config, iteration);
        *judge_secs_out = Some(judge_start.elapsed().as_secs_f64());
        if pass {
            *consecutive_judge_failures = 0;
            return JudgeEveryAction::Continue;
        }

        *consecutive_judge_failures += 1;
        if is_judge_bailout(*consecutive_judge_failures, config.max_judge_failures) {
            return JudgeEveryAction::Bailout;
        }
        return JudgeEveryAction::Continue;
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
fn fire_periodic_agents(runner: &mut LoopRunner, config: &Config, iteration: u32) {
    for periodic in &config.periodics {
        if iteration % periodic.cadence == 0 {
            eprintln!();
            let upper_name = capitalize_first(&periodic.name);
            render_section_banner(
                &upper_name,
                &format!("Periodic agent (iteration {:>4})", iteration),
                "\x1b[38;5;207m",
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

/// Run fire-and-forget hook commands. Non-zero exits warn but never stop the loop.
fn run_hooks(config: &Config, iteration: u32) {
    for cmd in &config.hooks {
        let result = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("YOKE_ITERATION", iteration.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();

        match result {
            Ok(status) if !status.success() => {
                log(&format!(
                    "{}WARNING: hook exited with {}: {}{}",
                    ORANGE,
                    status
                        .code()
                        .map_or("signal".to_string(), |c| c.to_string()),
                    cmd,
                    RESET
                ));
            }
            Err(e) => {
                log(&format!(
                    "{}WARNING: hook failed to execute: {} — {}{}",
                    ORANGE, cmd, e, RESET
                ));
            }
            Ok(_) => {}
        }
    }
}

/// Per-iteration data captured by `run_iteration_step` for metrics use.
struct IterationStepStats {
    guards_passed: bool,
    agent_secs: f64,
    stream: stream::StreamSummary,
    guards_secs: f64,
    guard_results: Vec<guard::GuardResult>,
}

/// Result of running the worker + guards for a single plan-loop iteration.
enum IterationStepResult {
    /// Worker + guards succeeded, loop should continue.
    Continue(IterationStepStats),
    /// Terminal outcome — return immediately from the loop.
    Terminal(PlanLoopOutcome),
}

/// Run the worker invocation, interrupt check, and guard evaluation for one iteration.
/// Returns `Continue` with guard results when the loop should proceed, or `Terminal`
/// when an early exit is needed.
fn run_iteration_step(
    runner: &mut LoopRunner,
    supervisor: &mut runtime::RunSupervisor,
    config: &Config,
    iteration: u32,
    prior_total: f64,
    dry_run: bool,
    protected: &[&str],
) -> IterationStepResult {
    if dry_run {
        log("(dry-run) Skipping OMP invocation");
    } else {
        let stats = invoke_agent(supervisor, config, iteration, prior_total);
        if !stats.success {
            log_error("OMP invocation failed — aborting loop");
            return IterationStepResult::Terminal(PlanLoopOutcome::Error);
        }
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return IterationStepResult::Terminal(PlanLoopOutcome::Interrupt);
        }
        if supervisor.shutdown_requested {
            restore_files(&runner.backup_dir, protected);
            log("Shutdown steering handled — stopping run");
            return IterationStepResult::Terminal(PlanLoopOutcome::Interrupt);
        }
        restore_files(&runner.backup_dir, protected);
        let guards_start = std::time::Instant::now();
        let guard_outcome = run_all_guards(config);
        let guards_secs = guards_start.elapsed().as_secs_f64();
        if guard_outcome.passed {
            log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));
        } else {
            log(&format!(
                "{}Some guards failed \u{2014} OMP will see results next iteration{}",
                ORANGE, RESET
            ));
        }
        return IterationStepResult::Continue(IterationStepStats {
            guards_passed: guard_outcome.passed,
            agent_secs: stats.wall_secs,
            stream: stats.stream,
            guards_secs,
            guard_results: guard_outcome.guard_results,
        });
    }

    // Dry-run path: run guards and exit after one iteration.
    restore_files(&runner.backup_dir, protected);
    let guard_outcome = run_all_guards(config);
    if guard_outcome.passed {
        log(&format!("{}{}All guards passed{}", GREEN, BOLD, RESET));
        log("(dry-run) Guards passed \u{2014} exiting after one iteration");
        IterationStepResult::Terminal(PlanLoopOutcome::Done)
    } else {
        log(&format!("{}Some guards failed{}", ORANGE, RESET));
        log("(dry-run) Exiting after one iteration");
        IterationStepResult::Terminal(PlanLoopOutcome::Error)
    }
}

/// Core plan-loop runner used by the brute loop.
///
/// - `config`: already-loaded Config
/// - `dry_run`: if true, skip OMP invocation (one iteration only)
/// - `recorder`: metrics recorder carrying run_id + project + output dir
fn run_plan_loop(
    config: &Config,
    dry_run: bool,
    recorder: &mut metrics::RunRecorder,
    run_artifacts: &mut artifacts::RunArtifacts,
    supervisor: &mut runtime::RunSupervisor,
) -> PlanLoopOutcome {
    if !dry_run {
        if let Err(e) = supervisor.ensure_control_started() {
            log_error(&e);
            return PlanLoopOutcome::Error;
        }
        supervisor.reset_worker_session();
    }

    let mut protected: Vec<&str> = vec![PROTOCOL_PATH, PLAN_PATH, CONF_PATH];
    if Path::new(JUDGE_PATH).exists() {
        protected.push(JUDGE_PATH);
    }
    let periodic_paths: Vec<String> = config.periodics.iter().map(|p| p.path.clone()).collect();
    for pp in &periodic_paths {
        protected.push(pp.as_str());
    }

    let backup_dir = backup_files(&protected);
    log(&format!("Backups in {}", backup_dir.display()));

    let mut runner = LoopRunner::new(backup_dir);
    supervisor.set_snapshot_target(&runner.backup_dir, &protected);
    let mut iteration: u32 = 0;
    let mut total_cost: f64 = 0.0;
    let mut consecutive_judge_failures: u32 = 0;

    loop {
        if signal::interrupted() {
            log("Interrupted \u{2014} shutting down");
            return PlanLoopOutcome::Interrupt;
        }
        iteration += 1;
        let iter_started_at = metrics::unix_now();
        let iter_wall_start = std::time::Instant::now();

        eprintln!();
        render_iteration_banner("Plan Iteration", iteration, true);

        if let Some((completed, total)) = stage_progress(PLAN_PATH) {
            eprintln!("{}", format_progress_bar(completed, total));
        }

        let restore_start = std::time::Instant::now();
        restore_files(&runner.backup_dir, &protected);
        let restore_ms = restore_start.elapsed().as_millis() as u64;

        let step_stats = match run_iteration_step(
            &mut runner,
            supervisor,
            config,
            iteration,
            total_cost,
            dry_run,
            &protected,
        ) {
            IterationStepResult::Continue(s) => s,
            IterationStepResult::Terminal(outcome) => return outcome,
        };
        total_cost += step_stats.stream.cost_usd;

        let periodics_start = std::time::Instant::now();
        fire_periodic_agents(&mut runner, config, iteration);
        let periodics_secs = periodics_start.elapsed().as_secs_f64();

        let hooks_start = std::time::Instant::now();
        run_hooks(config, iteration);
        let hooks_secs = hooks_start.elapsed().as_secs_f64();

        let mut judge_secs: Option<f64> = None;
        let mut early_return: Option<PlanLoopOutcome> = None;
        if let Some(judge_every) = config.judge_every {
            match evaluate_judge_every(
                &mut runner,
                config,
                iteration,
                step_stats.guards_passed,
                &mut consecutive_judge_failures,
                judge_every,
                &mut judge_secs,
            ) {
                JudgeEveryAction::Pass => early_return = Some(PlanLoopOutcome::JudgePass),
                JudgeEveryAction::Bailout => early_return = Some(PlanLoopOutcome::JudgeBailout),
                JudgeEveryAction::Continue => {}
            }
        }

        let status_done = step_stats.guards_passed && is_status_done(NOTES_PATH);

        // Build & flush iteration metrics row before any return path so
        // every completed iteration is recorded.
        let iter_metrics = metrics::IterationMetrics {
            run_id: recorder.ctx.run_id.clone(),
            project_slug: recorder.ctx.project_slug.clone(),
            mode: recorder.ctx.mode,
            iteration,
            started_at: iter_started_at,
            wall_secs: iter_wall_start.elapsed().as_secs_f64(),
            restore_ms,
            agent_secs: step_stats.agent_secs,
            guards_secs: step_stats.guards_secs,
            periodics_secs,
            hooks_secs,
            judge_secs,
            agent_reported_secs: step_stats.stream.agent_reported_secs,
            cost_usd: step_stats.stream.cost_usd,
            num_turns: step_stats.stream.num_turns,
            thinking_secs: step_stats.stream.thinking_secs,
            tool_counts: step_stats.stream.tool_counts.clone(),
            tool_durations_secs: step_stats.stream.tool_durations_secs.clone(),
            guards: step_stats
                .guard_results
                .iter()
                .map(|g| metrics::GuardRow {
                    name: g.name.clone(),
                    passed: g.passed,
                    skipped: g.skipped,
                    elapsed_secs: g.elapsed_secs,
                })
                .collect(),
            guards_passed: step_stats.guards_passed,
            status_done,
        };
        recorder.record_iteration(&iter_metrics);
        run_artifacts.record_iteration(&iter_metrics);

        if let Some(outcome) = early_return {
            return outcome;
        }

        if status_done {
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

/// Render the end-of-run metrics table to stderr.
fn render_run_summary(r: &metrics::RunMetrics) {
    eprintln!();
    eprintln!(
        "{}{}╔══════════════════════════════════════════════════════════════════╗{}",
        BOLD, ORANGE, RESET
    );
    eprintln!(
        "{}{}║  Run summary                                                     ║{}",
        BOLD, ORANGE, RESET
    );
    eprintln!(
        "{}{}╚══════════════════════════════════════════════════════════════════╝{}",
        BOLD, ORANGE, RESET
    );
    let dim = DIM;
    let reset = RESET;
    eprintln!("  {}mode:{}       {}", dim, reset, r.mode);
    eprintln!("  {}run_id:{}     {}", dim, reset, r.run_id);
    eprintln!("  {}project:{}    {}", dim, reset, r.project_slug);
    eprintln!("  {}outcome:{}    {}", dim, reset, r.outcome);
    eprintln!("  {}iterations:{} {}", dim, reset, r.iterations);
    eprintln!("  {}wall:{}       {:.1}s", dim, reset, r.total_wall_secs);
    eprintln!("  {}agent:{}      {:.1}s", dim, reset, r.total_agent_secs);
    eprintln!(
        "  {}thinking:{}   {:.1}s",
        dim, reset, r.total_thinking_secs
    );
    eprintln!("  {}guards:{}     {:.1}s", dim, reset, r.total_guards_secs);
    eprintln!("  {}cost:{}       ${:.2}", dim, reset, r.total_cost_usd);
    eprintln!();
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
const BRUTE_PROTECTED_FILES: &[&str] = &[PROTOCOL_PATH, PLAN_PATH, JUDGE_PATH, CONF_PATH];

fn run_brute_inner(dry_run: bool, supervisor: &mut runtime::RunSupervisor) -> BruteResult {
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
        config
            .image
            .as_ref()
            .map_or(String::from(", sandbox=off"), |img| format!(
                ", image={}",
                img
            ))
    ));

    // Preflight: required files, notes/archive, and non-empty judge spec.
    for path in &[PROTOCOL_PATH, PLAN_PATH, JUDGE_PATH, CONF_PATH] {
        if !Path::new(path).exists() {
            log_error(&format!("required file not found: {}", path));
            process::exit(1);
        }
    }
    if fs::read_to_string(JUDGE_PATH).is_ok_and(|content| content.trim().is_empty()) {
        log_error(&format!(
            "{} is empty — fill it in before running",
            JUDGE_PATH
        ));
        process::exit(1);
    }
    for path in &[NOTES_PATH, VERDICT_PATH] {
        if !Path::new(path).exists()
            && let Err(e) = fs::write(path, "")
        {
            log_error(&format!("cannot create {}: {}", path, e));
            process::exit(1);
        }
    }
    ensure_archive_dir();
    validate_loop_config(&config);
    log("Preflight OK");
    if !dry_run && let Err(e) = supervisor.ensure_control_started() {
        log_error(&e);
        return BruteResult::Error;
    }

    let mctx =
        metrics::MetricsContext::new(config.metrics_dir.clone(), "brute", config.metrics_enabled);
    if mctx.enabled {
        log(&format!(
            "Metrics: run_id={}, file={}",
            mctx.run_id,
            mctx.iteration_path().display()
        ));
    }
    let mut recorder = metrics::RunRecorder::new(mctx);
    recorder.on_finalize = Some(render_run_summary);

    let mut run_artifacts = artifacts::RunArtifacts::start(&recorder.ctx.run_id, "brute");
    for path in [
        CONF_PATH,
        PROTOCOL_PATH,
        BRIEFING_PATH,
        PLAN_PATH,
        JUDGE_PATH,
        NOTES_PATH,
    ] {
        run_artifacts.snapshot(path);
    }

    supervisor.start_summary(&config, &run_artifacts.run_id, run_artifacts.dir.as_path());

    let result = run_brute_core(
        &config,
        dry_run,
        &mut recorder,
        &mut run_artifacts,
        supervisor,
    );
    let outcome = match result {
        BruteResult::Pass => "judge_pass",
        BruteResult::Bailout => "bailout",
        BruteResult::Interrupt => "interrupt",
        BruteResult::Error => "error",
    };
    recorder.outcome = outcome;
    supervisor.finish_summary(outcome);
    run_artifacts.finish(outcome);
    result
}

/// Core brute loop logic.
///
/// - `config`: already-loaded Config
/// - `dry_run`: if true, skip OMP invocation (one iteration only)
/// - `recorder`: metrics context (shared with the plan loop)
/// - `run_artifacts`: per-run artifact store (shared with the plan loop)
fn run_brute_core(
    config: &Config,
    dry_run: bool,
    recorder: &mut metrics::RunRecorder,
    run_artifacts: &mut artifacts::RunArtifacts,
    supervisor: &mut runtime::RunSupervisor,
) -> BruteResult {
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
            match run_plan_loop(config, false, recorder, run_artifacts, supervisor) {
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
            let outcome = run_all_guards(config);
            if !outcome.passed {
                log(&format!("{}Guards failed{}", ORANGE, RESET));
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
        run_artifacts.record_judge(iteration, pass);

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
fn run_brute(dry_run: bool, supervisor: &mut runtime::RunSupervisor) -> i32 {
    match run_brute_inner(dry_run, supervisor) {
        BruteResult::Pass => 0,
        BruteResult::Bailout => 1,
        BruteResult::Interrupt => 130,
        BruteResult::Error => 1,
    }
}

/// Run a command that takes no arguments beyond `--help`.
fn cmd_simple(args: &[String], topic: &str, run: fn() -> i32) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        show_help(topic);
        process::exit(0);
    }
    if let Some(arg) = args.get(2) {
        log_error(&format!("unexpected argument '{}'", arg));
        show_help(topic);
        process::exit(2);
    }
    process::exit(run());
}

fn cmd_stash(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        stash::print_stash_help();
        process::exit(0);
    }
    let mode = "brute";
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
        show_help("layer");
        process::exit(0);
    }
    if args.get(2).is_some_and(|a| a == "--list") {
        println!("  repl");
        process::exit(0);
    }
    let names_arg = match args.get(2) {
        Some(a) => a.clone(),
        None => {
            log_error("missing layer name");
            show_help("layer");
            process::exit(2);
        }
    };
    process::exit(apply_layers(&names_arg));
}

/// Resolve the metrics root directory, preferring an existing yoke.conf
/// (so `metrics-dir` overrides apply) and falling back to the default
/// `~/.yoke/metrics/` when no config is present.
fn resolve_metrics_dir() -> PathBuf {
    if Path::new(CONF_PATH).exists() {
        if let Ok(c) = Config::load(Path::new(CONF_PATH)) {
            return c.metrics_dir;
        }
    }
    metrics::default_metrics_dir()
}

/// Read the value following `args[*i]` and advance past it.
fn take_value<'a>(args: &'a [String], i: &mut usize, flag: &str) -> &'a str {
    match args.get(*i + 1) {
        Some(value) => {
            *i += 2;
            value
        }
        None => {
            log_error(&format!("{flag} requires a value"));
            process::exit(2);
        }
    }
}

fn cmd_stats(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        show_help("stats");
        process::exit(0);
    }

    let mut last: Option<usize> = None;
    let mut project: Option<String> = None;
    let mut run_id: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--last" => {
                let v = take_value(args, &mut i, "--last");
                last = Some(v.parse().unwrap_or_else(|_| {
                    log_error(&format!("--last expects an integer, got '{v}'"));
                    process::exit(2)
                }));
            }
            "--project" => project = Some(take_value(args, &mut i, "--project").to_string()),
            "--run" => run_id = Some(take_value(args, &mut i, "--run").to_string()),
            other => {
                log_error(&format!("unknown flag '{}'", other));
                show_help("stats");
                process::exit(2);
            }
        }
    }

    let metrics_dir = resolve_metrics_dir();
    let slug = project.unwrap_or_else(metrics::project_slug);
    let slug_dir = metrics_dir.join(&slug);

    if let Some(rid) = run_id {
        process::exit(stats_show_run(&slug_dir, &rid, &slug));
    } else {
        process::exit(stats_list_runs(&slug_dir, &slug, last));
    }
}

fn stats_list_runs(slug_dir: &Path, slug: &str, last: Option<usize>) -> i32 {
    let mut runs = metrics::read_runs(slug_dir);
    if runs.is_empty() {
        log(&format!(
            "No runs recorded for project '{}' at {}",
            slug,
            slug_dir.display()
        ));
        log("Run `yoke run` to populate metrics.");
        return 0;
    }
    // run_id format is lex-sortable → desc gives most-recent first
    runs.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    if let Some(n) = last {
        runs.truncate(n);
    }

    log(&format!(
        "Runs for project '{}' ({} shown, source: {})",
        slug,
        runs.len(),
        slug_dir.display()
    ));
    println!();
    println!(
        "  {}{:<22}  {:<6}  {:<11}  {:>4}  {:>8}  {:>8}{}",
        BOLD, "RUN_ID", "MODE", "OUTCOME", "ITER", "WALL", "COST", RESET
    );
    for r in &runs {
        println!(
            "  {:<22}  {:<6}  {:<11}  {:>4}  {:>7.1}s  {:>7.2}",
            r.run_id, r.mode, r.outcome, r.iterations, r.total_wall_secs, r.total_cost_usd
        );
    }
    0
}

fn stats_show_run(slug_dir: &Path, run_id: &str, slug: &str) -> i32 {
    let path = slug_dir.join(format!("{}.ndjson", run_id));
    if !path.exists() {
        log_error(&format!("no metrics file at {}", path.display()));
        return 1;
    }
    let iters = metrics::read_iterations(&path);
    if iters.is_empty() {
        log(&format!(
            "Run {} has no iteration rows ({})",
            run_id,
            path.display()
        ));
        return 0;
    }
    log(&format!(
        "Run {} (project '{}', {} iteration{})",
        run_id,
        slug,
        iters.len(),
        if iters.len() == 1 { "" } else { "s" }
    ));
    println!();
    println!(
        "  {}{:>4}  {:>7}  {:>7}  {:>7}  {:>7}  {:>5}  {:>7}  {:>6}  {:>4}{}",
        BOLD, "ITER", "WALL", "AGENT", "THINK", "GUARDS", "TURNS", "COST", "GUARDS", "DONE", RESET
    );
    for r in &iters {
        println!(
            "  {:>4}  {:>6.1}s  {:>6.1}s  {:>6.1}s  {:>6.1}s  {:>5}  $ {:>5.2}  {:>6}  {:>4}",
            r.iteration,
            r.wall_secs,
            r.agent_secs,
            r.thinking_secs,
            r.guards_secs,
            r.num_turns,
            r.cost_usd,
            if r.guards_passed { "pass" } else { "fail" },
            if r.status_done { "yes" } else { "no" }
        );
    }
    0
}

fn cmd_inject(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        show_help("inject");
        process::exit(0);
    }

    let mut shutdown = false;
    let mut message: Option<String> = None;
    for arg in &args[2..] {
        match arg.as_str() {
            "--shutdown" => shutdown = true,
            other if other.starts_with('-') => {
                log_error(&format!("unknown flag '{}'", other));
                show_help("inject");
                process::exit(2);
            }
            other => {
                if message.is_some() {
                    log_error("inject expects exactly one message argument");
                    show_help("inject");
                    process::exit(2);
                }
                message = Some(other.to_string());
            }
        }
    }

    let Some(message) = message else {
        log_error("inject message is required");
        show_help("inject");
        process::exit(2);
    };
    if message.trim().is_empty() {
        log_error("inject message must not be empty");
        process::exit(2);
    }

    let kind = if shutdown {
        runtime::InjectionKind::Shutdown
    } else {
        runtime::InjectionKind::Normal
    };
    control_result(runtime::send_inject_command(kind, &message), "injection");
}

/// Log the outcome of a control request and exit with its status code.
fn control_result(response: Result<serde_json::Value, String>, label: &str) -> ! {
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    };
    if response["ok"].as_bool() == Some(true) {
        let status = response["status"].as_str().unwrap_or("accepted");
        match response["id"].as_str() {
            Some(id) => log(&format!("{label} {status} ({id})")),
            None => log(&format!("{label} {status}")),
        }
        process::exit(0);
    }
    log_error(
        response["error"]
            .as_str()
            .unwrap_or("active run rejected the request"),
    );
    process::exit(1);
}

fn cmd_control(args: &[String], action: &str) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        eprintln!("{}USAGE:{} yoke {action}", BOLD, RESET);
        process::exit(0);
    }
    if let Some(arg) = args.get(2) {
        log_error(&format!("unexpected argument '{}'", arg));
        process::exit(2);
    }
    control_result(runtime::send_command(action), action);
}

fn cmd_run(args: &[String]) -> ! {
    if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
        show_help("run");
        process::exit(0);
    }
    let mut dry_run = false;
    let mut _no_sandbox = false;
    let mut summary_fd: Option<i32> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--no-sandbox" => {
                _no_sandbox = true;
                i += 1;
            }
            "--summary-fd" => {
                let value = match args.get(i + 1) {
                    Some(value) => value,
                    None => {
                        log_error("--summary-fd requires a file descriptor");
                        process::exit(2);
                    }
                };
                summary_fd = Some(match value.parse::<i32>() {
                    Ok(fd) if fd >= 0 => fd,
                    _ => {
                        log_error(&format!(
                            "--summary-fd expects a non-negative integer, got '{}'",
                            value
                        ));
                        process::exit(2);
                    }
                });
                i += 2;
            }
            other => {
                log_error(&format!("unknown flag '{}'", other));
                show_help("run");
                process::exit(2);
            }
        }
    }
    if summary_fd.is_none()
        && let Ok(raw) = std::env::var("YOKE_SUMMARY_FD")
        && let Ok(fd) = raw.parse::<i32>()
    {
        summary_fd = Some(fd);
    }
    if let Err(e) = Config::load(Path::new(CONF_PATH)) {
        log_error(&e);
        process::exit(1);
    }
    let mut supervisor = runtime::RunSupervisor::new(summary_fd);
    let code = run_brute(dry_run, &mut supervisor);
    drop(supervisor);
    process::exit(code)
}

fn main() {
    signal::install();
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        show_help("");
        process::exit(2);
    }

    match args[1].as_str() {
        "init" => cmd_simple(&args, "init", init),
        "clean" => cmd_simple(&args, "clean", clean),
        "stash" => cmd_stash(&args),
        "layer" => cmd_layer(&args),
        "run" => cmd_run(&args),
        "inject" => cmd_inject(&args),
        "pause" => cmd_control(&args, "pause"),
        "resume" => cmd_control(&args, "resume"),
        "stop" => cmd_control(&args, "stop"),
        "stats" => cmd_stats(&args),
        "--help" | "-h" | "help" => {
            show_help("");
        }
        other => {
            log_error(&format!("unknown command '{}'", other));
            show_help("");
            process::exit(2);
        }
    }
}
