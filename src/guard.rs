//! Guard Orchestra — parallel guard runner that executes configured shell
//! commands, captures stdout/stderr, measures elapsed time, and produces
//! structured results for the box-drawn table in main.rs.
//!
//! All guards are spawned concurrently via threads. Results are collected
//! and returned in the original command order.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Instant;

pub struct GuardResult {
    pub name: String,
    pub passed: bool,
    pub skipped: bool,
    pub elapsed_secs: f64,
}

/// Keep only the last `max` lines of text.
fn tail_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    lines[lines.len() - max..].join("\n")
}

/// Execute a single guard command synchronously.
/// Returns (passed, raw_output, elapsed_secs).
fn run_one(cmd: &str) -> (bool, String, f64) {
    let start = Instant::now();
    let output = Command::new("sh").arg("-c").arg(cmd).output();
    let elapsed_secs = start.elapsed().as_secs_f64();

    let (exit_ok, raw_output) = match output {
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            (o.status.success(), combined)
        }
        Err(e) => (false, format!("failed to execute: {}", e)),
    };

    (exit_ok, raw_output, elapsed_secs)
}

/// Run all configured guard commands in parallel.
/// Each guard is spawned on its own thread. Results are collected in the
/// original command order and written to `results_path` in markdown format.
///
/// The rendered box-drawn table with PASS/FAIL/SKIPPED status and timing
/// is handled by the caller in main.rs.
pub fn run_guards(guards: &[String], max_tail: usize, results_path: &Path) -> Vec<GuardResult> {
    if guards.is_empty() {
        let _ = fs::write(results_path, "");
        return Vec::new();
    }

    // Spawn all guards in parallel
    let handles: Vec<_> = guards
        .iter()
        .map(|cmd| {
            let cmd = cmd.clone();
            thread::spawn(move || {
                if crate::signal::interrupted() {
                    return (cmd, false, String::new(), true, 0.0);
                }
                let (passed, raw_output, elapsed) = run_one(&cmd);
                (cmd, passed, raw_output, false, elapsed)
            })
        })
        .collect();

    // Collect results in order
    let mut results = Vec::with_capacity(guards.len());
    let mut markdown = String::new();

    for handle in handles {
        let (name, passed, raw_output, skipped, elapsed_secs) = match handle.join() {
            Ok(r) => r,
            Err(_) => {
                // Thread panicked — treat as failure
                (
                    String::from("(unknown)"),
                    false,
                    String::from("guard thread panicked"),
                    false,
                    0.0,
                )
            }
        };

        let truncated = tail_lines(&raw_output, max_tail);

        if skipped {
            markdown.push_str(&format!("## {} \u{2014} SKIPPED\n\n", name));
            markdown.push_str("```\nSkipped due to interrupt.\n```\n\n");
        } else {
            let status_label = if passed { "PASS" } else { "FAIL" };
            markdown.push_str(&format!("## {} \u{2014} {}\n\n", name, status_label));
            markdown.push_str("```\n");
            markdown.push_str(&truncated);
            if !truncated.is_empty() && !truncated.ends_with('\n') {
                markdown.push('\n');
            }
            markdown.push_str("```\n\n");
        }

        results.push(GuardResult {
            name,
            passed,
            skipped,
            elapsed_secs,
        });
    }

    // Write results file
    if let Err(e) = fs::write(results_path, &markdown) {
        eprintln!("[yoke] WARNING: failed to write guard results: {}", e);
    }

    results
}
