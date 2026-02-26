use std::fs;
use std::path::Path;
use std::process::Command;

pub struct GuardResult {
    pub name: String,
    pub passed: bool,
    pub output: String,
    pub skipped: bool,
}

/// Keep only the last `max` lines of text.
fn tail_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    lines[lines.len() - max..].join("\n")
}

/// Run all configured guard commands in order (fail-fast).
/// Writes results to `results_path` in markdown format.
pub fn run_guards(guards: &[String], max_tail: usize, results_path: &Path) -> Vec<GuardResult> {
    let mut results = Vec::new();
    let mut markdown = String::new();
    let mut failed = false;

    for cmd in guards {
        if failed {
            let result = GuardResult {
                name: cmd.clone(),
                passed: false,
                output: String::new(),
                skipped: true,
            };
            markdown.push_str(&format!("## {} — SKIPPED\n\n", cmd));
            markdown.push_str("```\nSkipped due to earlier guard failure.\n```\n\n");
            results.push(result);
            continue;
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output();

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

        let truncated = tail_lines(&raw_output, max_tail);
        let status_label = if exit_ok { "PASS" } else { "FAIL" };

        markdown.push_str(&format!("## {} — {}\n\n", cmd, status_label));
        markdown.push_str("```\n");
        markdown.push_str(&truncated);
        if !truncated.is_empty() && !truncated.ends_with('\n') {
            markdown.push('\n');
        }
        markdown.push_str("```\n\n");

        if !exit_ok {
            failed = true;
        }

        results.push(GuardResult {
            name: cmd.clone(),
            passed: exit_ok,
            output: truncated,
            skipped: false,
        });
    }

    // Write results file
    if let Err(e) = fs::write(results_path, &markdown) {
        eprintln!("[ci] WARNING: failed to write guard results: {}", e);
    }

    results
}
