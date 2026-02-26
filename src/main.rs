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

const CONF_PATH: &str = "loop/ci.conf";
const NOTES_PATH: &str = "loop/notes.md";
const GUARD_RESULTS_PATH: &str = "loop/guard-results.md";
const PROTOCOL_PATH: &str = "loop/protocol.md";
const PLAN_PATH: &str = "loop/plan.md";

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
    eprintln!("{}{}[ci]{} {}", BOLD, CYAN, RESET, msg);
}

fn log_error(msg: &str) {
    eprintln!("{}{}[ci] ERROR:{} {}", BOLD, RED, RESET, msg);
}

fn print_usage() {
    eprintln!(
        "{}{}ci{} — LLM loop harness{}",
        BOLD, CYAN, RESET, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} ci <command> [options]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}COMMANDS:{}", BOLD, RESET);
    eprintln!(
        "  {}run{}              Launch the CI loop (invoke Claude, run guards, iterate)",
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
        "{}{}ci run{} — execute the CI loop{}",
        BOLD, CYAN, RESET, RESET
    );
    eprintln!();
    eprintln!(
        "{}USAGE:{} ci run [--dry-run]",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("{}OPTIONS:{}", BOLD, RESET);
    eprintln!("  --dry-run    Run one iteration without invoking Claude");
    eprintln!("               (boundary check + configured guards only)");
    eprintln!();
    eprintln!("{}WORKFLOW:{}", BOLD, RESET);
    eprintln!("  1. Load config from {}", CONF_PATH);
    eprintln!("  2. Backup protected files (protocol.md, plan.md, ci.conf)");
    eprintln!("  3. Per iteration:");
    eprintln!("     a. Restore protected files");
    eprintln!("     b. Invoke Claude with stream-json output");
    eprintln!("     c. Run diff boundary check");
    eprintln!("     d. Run configured guard commands");
    eprintln!("  4. Exit when STATUS: DONE and all guards pass");
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
    let backup_dir = std::env::temp_dir().join(format!("ci-backup-{}", process::id()));
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
                    "{}{}[ci] WARNING:{} failed to restore {}: {}",
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
    log(&format!("Launching Claude (iteration {})...", iteration));

    let mut child = match Command::new(&config.claude_bin)
        .args([
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "-p",
            "Read loop/protocol.md and follow its instructions.",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_error(&format!(
                "failed to spawn '{}': {}",
                config.claude_bin, e
            ));
            return false;
        }
    };

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
    runner.child = None;

    log(&format!(
        "Claude exited ({})",
        if status { "success" } else { "failure" }
    ));
    status
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
        "Config loaded: claude={}, max_tail={}, {} scope rules, {} guards",
        config.claude_bin,
        config.max_tail,
        config.scope_rules.len(),
        config.guards.len()
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
        } else {
            invoke_claude(&mut runner, &config, iteration);
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

fn main() {
    signal::install();
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    match args[1].as_str() {
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
            process::exit(run_loop(dry_run));
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
