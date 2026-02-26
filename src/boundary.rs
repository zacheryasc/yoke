use std::process::Command;

use crate::config::{Config, ScopeTag};

pub struct BoundaryResult {
    pub passed: bool,
    pub violations: Vec<String>,
}

/// Collect all changed files from git (staged, unstaged, and untracked).
/// Returns (changed_files, deleted_files, modified_files).
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
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split('\t');
        let status = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        let file = match parts.next() {
            Some(f) => f.to_string(),
            None => continue,
        };

        match status {
            "D" => {
                deleted.push(file.clone());
                changed.push(file);
            }
            "M" => {
                modified.push(file.clone());
                changed.push(file);
            }
            s if s.starts_with('R') => {
                // Rename: old path treated as delete, new path as add
                deleted.push(file.clone());
                changed.push(file);
                if let Some(new_file) = parts.next() {
                    changed.push(new_file.to_string());
                }
            }
            _ => {
                // A (added) or other statuses
                changed.push(file);
            }
        }
    }

    // git ls-files --others --exclude-standard (untracked files)
    let output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .map_err(|e| format!("failed to run git ls-files: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let file = line.trim();
        if !file.is_empty() {
            changed.push(file.to_string());
        }
    }

    Ok((changed, deleted, modified))
}

/// Check if a file's diff contains removed lines (lines starting with '-' but not '---').
fn has_removed_lines(file: &str) -> bool {
    let output = Command::new("git")
        .args(["diff", "HEAD", "--", file])
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                if line.starts_with('-') && !line.starts_with("---") {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
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
        let tag = config.resolve_tag(file);

        match tag {
            None => {
                violations.push(format!("DENY: {} (no matching rule)", file));
            }
            Some(ScopeTag::NoModify) => {
                violations.push(format!("NO-MODIFY: {} (protected by no-modify)", file));
            }
            Some(ScopeTag::AddOnly) => {
                // Deleted files are never add-only safe
                if deleted.iter().any(|d| d == file) {
                    violations.push(format!("ADD-ONLY: {} (deletion not allowed)", file));
                    continue;
                }
                // Modified tracked files: check for removed lines
                if modified.iter().any(|m| m == file) && has_removed_lines(file) {
                    violations.push(format!("ADD-ONLY: {} (removed lines detected)", file));
                }
                // New/untracked files are purely additive — always OK
            }
            Some(ScopeTag::Allow) => {
                // anything goes
            }
        }
    }

    BoundaryResult {
        passed: violations.is_empty(),
        violations,
    }
}
