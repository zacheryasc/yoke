//! Per-run artifacts under `.loop/runs/<run-id>/`.
//!
//! Each run gets a self-contained directory:
//!
//! ```text
//! .loop/runs/<run-id>/
//!   run.json              run manifest (outcome, iterations, VCS pointers)
//!   summary.md            human entry point
//!   snapshots/            plan/judge/protocol/config/notes at run start
//!   iterations/0001/      one directory per plan iteration
//!     meta.json
//!     diff.patch          cumulative diff from the run's base commit
//!     diffstat.txt
//!     guard-results.md
//!     notes.md
//!     verdict.md          judge verdict visible at that point
//!   judge/attempt-0001.md standalone judge verdicts
//!   final.diff            cumulative diff at run end
//!   final-verdict.md
//!   final-notes.md
//!   archive/              verbatim copy of .loop/archive at run end
//! ```
//!
//! Diffs are captured through a temporary Git index, so the real index and
//! the user's branch are never touched. The run manifest records the base
//! branch and commit, plus the current branch/HEAD at each capture.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{log, log_error, metrics};

const RUNS_DIR: &str = ".loop/runs";
const GUARD_RESULTS_PATH: &str = ".loop/guard-results.md";
const NOTES_PATH: &str = ".loop/notes.md";
const VERDICT_PATH: &str = ".loop/verdict.md";

struct GitInfo {
    root: PathBuf,
    base_branch: String,
    base_sha: Option<String>,
    /// Commit or empty-tree object used as the diff base.
    base_ref: String,
    dirty_at_start: bool,
}

pub struct RunArtifacts {
    enabled: bool,
    pub(crate) dir: PathBuf,
    pub(crate) run_id: String,
    mode: String,
    project: String,
    started_at: u64,
    sequence: u32,
    git: Option<GitInfo>,
}

impl RunArtifacts {
    /// Create the artifact directory, snapshot the VCS base, and write the
    /// initial manifest. Artifact failures are logged but never stop a run.
    pub fn start(run_id: &str, mode: &str) -> Self {
        let dir = Path::new(RUNS_DIR).join(run_id);
        let mut artifacts = Self {
            enabled: true,
            dir,
            run_id: run_id.to_string(),
            mode: mode.to_string(),
            project: metrics::project_slug(),
            started_at: metrics::unix_now(),
            sequence: 0,
            git: detect_git(),
        };

        for sub in ["iterations", "snapshots", "judge"] {
            if let Err(e) = fs::create_dir_all(artifacts.dir.join(sub)) {
                log_error(&format!(
                    "failed to create run artifacts at {}: {}",
                    artifacts.dir.display(),
                    e
                ));
                artifacts.enabled = false;
                return artifacts;
            }
        }

        artifacts.write_manifest(None, 0);
        log(&format!("Run artifacts: {}", artifacts.dir.display()));
        artifacts
    }

    pub fn snapshot(&mut self, path: &str) {
        if !self.enabled {
            return;
        }
        let src = Path::new(path);
        let Some(name) = src.file_name() else {
            return;
        };
        if src.is_file()
            && let Err(e) = fs::copy(src, self.dir.join("snapshots").join(name))
        {
            log_error(&format!("failed to snapshot {}: {}", src.display(), e));
        }
    }

    /// Record one completed plan iteration. `metrics::IterationMetrics`
    /// carries the timings/cost; the artifacts add the diff and file
    /// snapshots that the metrics rows intentionally omit.
    pub fn record_iteration(&mut self, m: &metrics::IterationMetrics) {
        if !self.enabled {
            return;
        }
        self.sequence += 1;
        let iter_dir = self
            .dir
            .join("iterations")
            .join(format!("{:04}", self.sequence));
        if let Err(e) = fs::create_dir_all(&iter_dir) {
            log_error(&format!(
                "failed to create iteration artifacts {}: {}",
                iter_dir.display(),
                e
            ));
            return;
        }

        let guards: Vec<serde_json::Value> = m
            .guards
            .iter()
            .map(|g| {
                serde_json::json!({
                    "name": g.name,
                    "passed": g.passed,
                    "skipped": g.skipped,
                    "elapsed_secs": round3(g.elapsed_secs),
                })
            })
            .collect();

        let meta = serde_json::json!({
            "sequence": self.sequence,
            "iteration": m.iteration,
            "started_at": m.started_at,
            "wall_secs": round3(m.wall_secs),
            "restore_ms": m.restore_ms,
            "agent_secs": round3(m.agent_secs),
            "guards_secs": round3(m.guards_secs),
            "periodics_secs": round3(m.periodics_secs),
            "hooks_secs": round3(m.hooks_secs),
            "judge_secs": m.judge_secs.map(round3),
            "cost_usd": m.cost_usd,
            "num_turns": m.num_turns,
            "thinking_secs": round3(m.thinking_secs),
            "guards_passed": m.guards_passed,
            "status_done": m.status_done,
            "guards": guards,
            "vcs": self.current_vcs(),
        });
        if let Err(e) = write_json(&iter_dir.join("meta.json"), &meta) {
            log_error(&e);
        }

        copy_if_exists(
            Path::new(GUARD_RESULTS_PATH),
            &iter_dir.join("guard-results.md"),
        );
        copy_if_exists(Path::new(NOTES_PATH), &iter_dir.join("notes.md"));
        copy_if_exists(Path::new(VERDICT_PATH), &iter_dir.join("verdict.md"));
        self.capture_diff(&iter_dir.join("diff.patch"), &iter_dir.join("diffstat.txt"));
    }

    /// Record a standalone judge verdict from one brute attempt.
    pub fn record_judge(&mut self, attempt: u32, pass: bool) {
        if !self.enabled {
            return;
        }
        let stem = format!("attempt-{:04}", attempt);
        copy_if_exists(
            Path::new(VERDICT_PATH),
            &self.dir.join("judge").join(format!("{stem}.md")),
        );
        let meta = serde_json::json!({
            "attempt": attempt,
            "pass": pass,
            "vcs": self.current_vcs(),
        });
        if let Err(e) = write_json(&self.dir.join("judge").join(format!("{stem}.json")), &meta) {
            log_error(&e);
        }
    }

    /// Capture the final patch/state and close the run manifest.
    pub fn finish(&mut self, outcome: &str) {
        if !self.enabled {
            return;
        }
        self.capture_diff(
            &self.dir.join("final.diff"),
            &self.dir.join("final.diffstat.txt"),
        );
        copy_if_exists(Path::new(VERDICT_PATH), &self.dir.join("final-verdict.md"));
        copy_if_exists(Path::new(NOTES_PATH), &self.dir.join("final-notes.md"));
        copy_dir(Path::new(".loop/archive"), &self.dir.join("archive"));

        self.write_manifest(Some(outcome), self.sequence);
        if let Err(e) = write_summary(self, outcome) {
            log_error(&e);
        }
    }

    fn current_vcs(&self) -> serde_json::Value {
        match &self.git {
            None => serde_json::json!({"kind": "none"}),
            Some(git) => {
                let branch = git_output(&git.root, &["symbolic-ref", "--short", "-q", "HEAD"])
                    .unwrap_or_else(|| "(detached)".to_string());
                let head_sha = git_output(&git.root, &["rev-parse", "HEAD"]);
                serde_json::json!({
                    "kind": "git",
                    "root": git.root,
                    "branch": branch,
                    "head_sha": head_sha,
                    "base_branch": git.base_branch,
                    "base_sha": git.base_sha,
                })
            }
        }
    }

    fn capture_diff(&self, patch_path: &Path, stat_path: &Path) {
        let result = self.write_diff(patch_path, stat_path);
        if let Err(e) = result {
            log_error(&format!("failed to capture diff: {}", e));
        }
    }

    fn write_diff(&self, patch_path: &Path, stat_path: &Path) -> Result<(), String> {
        let Some(git) = &self.git else {
            return Ok(());
        };

        if let Some(parent) = patch_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }

        let index = self.dir.join(".diff-index");
        let _ = fs::remove_file(&index);

        let run = |args: &[&str]| -> Result<Vec<u8>, String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&git.root)
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .output()
                .map_err(|e| format!("failed to run git: {}", e))?;
            if !out.status.success() {
                return Err(format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(out.stdout)
        };

        run(&["read-tree", &git.base_ref])?;
        // Stage everything into the temporary index, then drop `.loop` from
        // it. A negative pathspec on an ignored `.loop` makes `git add` fail.
        run(&["add", "-A", "--", "."])?;
        run(&[
            "rm",
            "-r",
            "--cached",
            "--force",
            "--ignore-unmatch",
            ".loop",
        ])?;
        let tree = String::from_utf8(run(&["write-tree"])?)
            .map_err(|e| format!("git write-tree output was not UTF-8: {}", e))?
            .trim()
            .to_string();

        let diff = run(&["diff", "--binary", "--full-index", &git.base_ref, &tree])?;
        fs::write(patch_path, &diff)
            .map_err(|e| format!("failed to write {}: {}", patch_path.display(), e))?;

        let stat = run(&["diff", "--stat", &git.base_ref, &tree]).unwrap_or_default();
        let _ = fs::write(stat_path, stat);

        let _ = fs::remove_file(&index);
        Ok(())
    }

    fn write_manifest(&self, outcome: Option<&str>, iterations: u32) {
        let ended_at = outcome.map(|_| metrics::unix_now());
        let mut vcs = self.current_vcs();
        if let (Some(git), Some(obj)) = (&self.git, vcs.as_object_mut()) {
            obj.insert(
                "dirty_at_start".to_string(),
                serde_json::Value::Bool(git.dirty_at_start),
            );
        }
        let manifest = serde_json::json!({
            "run_id": &self.run_id,
            "mode": &self.mode,
            "project": &self.project,
            "started_at": self.started_at,
            "ended_at": ended_at,
            "outcome": outcome,
            "iterations": iterations,
            "artifacts": self.dir,
            "vcs": vcs,
        });
        if let Err(e) = write_json(&self.dir.join("run.json"), &manifest) {
            log_error(&e);
        }
    }
}

fn write_summary(artifacts: &RunArtifacts, outcome: &str) -> Result<(), String> {
    let vcs = match &artifacts.git {
        Some(git) => format!(
            "base `{}` at `{}`",
            git.base_branch,
            git.base_sha.as_deref().unwrap_or("(no commits)")
        ),
        None => "no git repository".to_string(),
    };
    let body = format!(
        "# Yoke run {}\n\n\
         - outcome: `{}`\n\
         - project: `{}`\n\
         - iterations: {}\n\
         - started: {}\n\
         - vcs: {}\n\n\
         ## Files\n\n\
         - `run.json` — run manifest\n\
         - `final.diff` — cumulative change from the base commit\n\
         - `iterations/` — per-iteration metadata, diff, guards, notes\n\
         - `judge/` — standalone judge verdicts\n\
         - `snapshots/` — plan/judge/protocol/config at run start\n\
         - `final-verdict.md`, `final-notes.md`, `archive/` — end state\n",
        artifacts.run_id, outcome, artifacts.project, artifacts.sequence, artifacts.started_at, vcs
    );
    let path = artifacts.dir.join("summary.md");
    fs::write(&path, body).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let data = serde_json::to_string_pretty(value)
        .map_err(|e| format!("failed to serialize {}: {}", path.display(), e))?;
    fs::write(path, format!("{}\n", data))
        .map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn copy_if_exists(src: &Path, dest: &Path) {
    if src.is_file()
        && let Some(parent) = dest.parent()
        && let Err(e) = fs::create_dir_all(parent).and_then(|_| fs::copy(src, dest))
    {
        log_error(&format!(
            "failed to copy {} to {}: {}",
            src.display(),
            dest.display(),
            e
        ));
    }
}

fn copy_dir(src: &Path, dest: &Path) {
    if !src.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else if let Some(parent) = target.parent()
            && let Err(e) = fs::create_dir_all(parent).and_then(|_| fs::copy(&path, &target))
        {
            log_error(&format!(
                "failed to copy {} to {}: {}",
                path.display(),
                target.display(),
                e
            ));
        }
    }
}

fn detect_git() -> Option<GitInfo> {
    let root = PathBuf::from(git_output(
        Path::new("."),
        &["rev-parse", "--show-toplevel"],
    )?);
    let base_branch = git_output(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .unwrap_or_else(|| "(detached)".to_string());
    let base_sha = git_output(&root, &["rev-parse", "HEAD"]);
    let empty_tree = git_output(&root, &["hash-object", "-t", "tree", "/dev/null"])?;
    let base_ref = base_sha.clone().unwrap_or(empty_tree);
    let dirty_at_start = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain", "--", ".", ":(exclude).loop"])
        .output()
        .is_ok_and(|out| out.status.success() && !out.stdout.is_empty());

    Some(GitInfo {
        root,
        base_branch,
        base_sha,
        base_ref,
        dirty_at_start,
    })
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
