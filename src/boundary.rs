use std::process::Command;

use crate::config::{Config, ScopeTag};

pub struct BoundaryResult {
    pub passed: bool,
    pub violations: Vec<String>,
}

/// Collect all changed files from git (staged, unstaged, and untracked).
/// Returns (changed_files, deleted_files, modified_files).
#[allow(clippy::type_complexity)]
fn collect_changes() -> Result<(Vec<String>, Vec<String>, Vec<String>), String> {
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    let mut modified = Vec::new();

    // git diff --name-status HEAD
    let output = Command::new("git")
        .args(["diff", "--name-status", "HEAD"])
        .output()
        .map_err(|e| format!("failed to run git diff: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.trim().split('\t');
        let (Some(status), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        changed.push(file.to_owned());

        match status {
            "D" => deleted.push(file.to_owned()),
            "M" => modified.push(file.to_owned()),
            s if s.starts_with('R') => {
                deleted.push(file.to_owned());
                changed.extend(parts.next().map(str::to_owned));
            }
            _ => {}
        }
    }

    // git ls-files --others --exclude-standard (untracked files)
    let output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .map_err(|e| format!("failed to run git ls-files: {}", e))?;

    changed.extend(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|file| !file.is_empty())
            .map(str::to_owned),
    );

    Ok((changed, deleted, modified))
}

/// Check if a file's diff contains removed lines (lines starting with '-' but not '---').
fn has_removed_lines(file: &str) -> bool {
    let output = Command::new("git")
        .args(["diff", "HEAD", "--", file])
        .output();

    output.is_ok_and(|out| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.starts_with('-') && !line.starts_with("---"))
    })
}

/// Run the diff boundary check against the config's scope rules.
pub fn check(config: &Config) -> BoundaryResult {
    let (changed, deleted, modified) = match collect_changes() {
        Ok(c) => c,
        Err(e) => {
            return BoundaryResult {
                passed: false,
                violations: vec![format!("failed to collect changes: {}", e)],
            };
        }
    };

    let mut violations = Vec::new();

    for file in &changed {
        // Skip .loop/ files — they are harness infrastructure, not user code.
        // The agent writes notes.md, and the harness itself writes
        // guard-results.md and verdict.md.
        if file.starts_with(".loop/") {
            continue;
        }

        let reason = match config.resolve_tag(file) {
            None => format!("DENY: {file} (no matching rule)"),
            Some(ScopeTag::NoModify) => format!("NO-MODIFY: {file} (protected by no-modify)"),
            Some(ScopeTag::AddOnly) if deleted.contains(file) => {
                format!("ADD-ONLY: {file} (deletion not allowed)")
            }
            Some(ScopeTag::AddOnly) if modified.contains(file) && has_removed_lines(file) => {
                format!("ADD-ONLY: {file} (removed lines detected)")
            }
            _ => continue,
        };
        violations.push(reason);
    }

    BoundaryResult {
        passed: violations.is_empty(),
        violations,
    }
}
