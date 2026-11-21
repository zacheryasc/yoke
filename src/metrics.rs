//! Persistent metrics for yoke runs.  Rows are written under `~/.yoke/metrics/`
//! (or the configured `metrics-dir`) as NDJSON — one row per iteration, one
//! row per completed run.  Storage lives outside the project tree so it
//! survives `yoke clean`, `yoke stash`, branch resets, and project deletes.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(serde::Serialize)]
pub struct IterationMetrics {
    pub run_id: String,
    pub project_slug: String,
    pub mode: &'static str,
    pub iteration: u32,
    pub started_at: u64,

    pub wall_secs: f64,
    pub restore_ms: u64,
    pub agent_secs: f64,
    pub guards_secs: f64,
    pub periodics_secs: f64,
    pub hooks_secs: f64,
    pub judge_secs: Option<f64>,

    pub agent_reported_secs: Option<f64>,
    pub cost_usd: f64,
    pub num_turns: u32,

    pub thinking_secs: f64,

    pub tool_counts: BTreeMap<String, u32>,
    pub tool_durations_secs: BTreeMap<String, f64>,

    pub guards: Vec<crate::guard::GuardResult>,

    pub guards_passed: bool,
    pub status_done: bool,
}

#[derive(serde::Serialize)]
pub struct RunMetrics {
    pub run_id: String,
    pub project_slug: String,
    pub mode: &'static str,
    pub started_at: u64,
    pub ended_at: u64,
    pub iterations: u32,
    pub outcome: &'static str,

    pub total_wall_secs: f64,
    pub total_agent_secs: f64,
    pub total_thinking_secs: f64,
    pub total_guards_secs: f64,
    pub total_cost_usd: f64,
}

/// Accumulates iteration totals and writes the run row on Drop, including
/// on the first SIGINT. A second SIGINT exits without unwinding.
///
/// `output_dir` is `None` when metrics are disabled; totals are still kept
/// for the terminal summary.
pub struct RunRecorder {
    pub row: RunMetrics,
    output_dir: Option<PathBuf>,
    wall_start: std::time::Instant,
    /// Finalized-run callback: lets the caller render an end-of-run summary
    /// table without making Drop know about ANSI rendering.
    pub on_finalize: Option<fn(&RunMetrics)>,
}

impl RunRecorder {
    pub fn new(root_dir: Option<&Path>, mode: &'static str) -> Self {
        let project_slug = project_slug();
        let output_dir = root_dir.map(|dir| dir.join(&project_slug));
        Self {
            row: RunMetrics {
                run_id: new_run_id(),
                project_slug,
                mode,
                started_at: unix_now(),
                ended_at: 0,
                total_wall_secs: 0.0,
                iterations: 0,
                total_agent_secs: 0.0,
                total_thinking_secs: 0.0,
                total_guards_secs: 0.0,
                total_cost_usd: 0.0,
                // Default to "interrupt" so an unwinding stack still records
                // something meaningful — clean exit paths override this.
                outcome: "interrupt",
            },
            output_dir,
            wall_start: std::time::Instant::now(),
            on_finalize: None,
        }
    }

    /// Write the iteration row and accumulate its totals in one step.
    pub fn record_iteration(&mut self, m: &IterationMetrics) {
        if let Some(path) = self.iteration_path() {
            append_line(&path, serialize_row(m));
        }
        self.row.iterations += 1;
        self.row.total_agent_secs += m.agent_secs;
        self.row.total_thinking_secs += m.thinking_secs;
        self.row.total_guards_secs += m.guards_secs;
        self.row.total_cost_usd += m.cost_usd;
    }

    pub fn iteration_path(&self) -> Option<PathBuf> {
        Some(
            self.output_dir
                .as_ref()?
                .join(format!("{}.ndjson", self.row.run_id)),
        )
    }
}

impl Drop for RunRecorder {
    fn drop(&mut self) {
        self.row.ended_at = unix_now();
        self.row.total_wall_secs = self.wall_start.elapsed().as_secs_f64();
        if let Some(dir) = &self.output_dir {
            append_line(&dir.join("runs.ndjson"), serialize_row(&self.row));
        }
        if let Some(cb) = self.on_finalize.take() {
            cb(&self.row);
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sortable run id: `YYYYMMDDTHHMMSS-NNNNNN`, where the suffix is the
/// six-digit sub-second microsecond count.  Lexicographic sort matches
/// chronological order.  No external time crate dep.
pub fn new_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let micros = now.subsec_micros();
    let (y, m, d, hh, mm, ss) = unix_to_utc(secs);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}-{:06}",
        y, m, d, hh, mm, ss, micros
    )
}

/// Convert unix-seconds-since-epoch (UTC) to (year, month, day, hour, minute, second).
/// Howard Hinnant's date algorithm — integer-only, no leap-second handling.
pub(crate) fn unix_to_utc(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let hh = (tod / 3600) as u32;
    let mm = ((tod % 3600) / 60) as u32;
    let ss = (tod % 60) as u32;

    let z = days + 719468;
    let era = z / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m as u32, d as u32, hh, mm, ss)
}

/// Stable identifier for the current working directory: basename + FNV-1a
/// hash of the absolute path.  Two checkouts with the same basename get
/// different slugs.
pub fn project_slug() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let abs = cwd.canonicalize().unwrap_or(cwd);
    let base: String = abs
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let hash = abs
        .to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0x811c_9dc5u32, |h, &b| {
            (h ^ b as u32).wrapping_mul(0x0100_0193)
        });
    format!("{}-{:08x}", base, hash)
}

/// Expand a leading `~` or `~/` against `$HOME`.  Returns the input
/// unchanged if no tilde prefix or no HOME.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if s == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

pub fn default_metrics_dir() -> PathBuf {
    expand_tilde("~/.yoke/metrics")
}

fn append_line(path: &Path, mut line: String) {
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        eprintln!(
            "[yoke] WARNING: cannot create metrics dir {}: {}",
            parent.display(),
            e
        );
        return;
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            // Single write_all keeps the row atomic on POSIX for small lines.
            line.push('\n');
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("[yoke] WARNING: failed to write metrics row: {}", e);
            }
        }
        Err(e) => {
            eprintln!(
                "[yoke] WARNING: cannot open metrics file {}: {}",
                path.display(),
                e
            );
        }
    }
}

// ─── JSON emission ────────────────────────────────────────────────────────

fn serialize_row(row: &impl serde::Serialize) -> String {
    serde_json::to_string(row).expect("metrics row is JSON-serializable")
}

// ─── Read side: parse rows for `yoke stats` ──────────────────────────────
//
// The write side uses `&'static str` for mode/outcome — small win on the
// hot path.  The read side comes from runtime data so it uses owned
// `String` fields and a separate row type.  Keeps both sides honest
// without forcing one to bend to the other.

/// One parsed row from `runs.ndjson`.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub mode: String,
    pub iterations: u32,
    pub outcome: String,
    pub total_wall_secs: f64,
    pub total_cost_usd: f64,
}

/// One parsed row from `<run-id>.ndjson`.
#[derive(Debug, Clone)]
pub struct IterRow {
    pub iteration: u32,
    #[allow(dead_code)] // parsed for row completeness; used by unit tests
    pub mode: String,
    pub wall_secs: f64,
    pub agent_secs: f64,
    pub thinking_secs: f64,
    pub guards_secs: f64,
    pub num_turns: u32,
    pub cost_usd: f64,
    pub guards_passed: bool,
    pub status_done: bool,
}

pub fn read_runs(slug_dir: &Path) -> Vec<RunRow> {
    read_ndjson(&slug_dir.join("runs.ndjson"), parse_run_row)
}

pub fn read_iterations(file: &Path) -> Vec<IterRow> {
    read_ndjson(file, parse_iter_row)
}

fn read_ndjson<T>(path: &Path, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(parse)
        .collect()
}

fn parse_run_row(line: &str) -> Option<RunRow> {
    use crate::json::{extract_num, extract_str};
    Some(RunRow {
        run_id: extract_str(line, "run_id")?.to_string(),
        mode: extract_str(line, "mode")?.to_string(),
        iterations: extract_num(line, "iterations")? as u32,
        outcome: extract_str(line, "outcome")?.to_string(),
        total_wall_secs: extract_num(line, "total_wall_secs").unwrap_or(0.0),
        total_cost_usd: extract_num(line, "total_cost_usd").unwrap_or(0.0),
    })
}

fn parse_iter_row(line: &str) -> Option<IterRow> {
    use crate::json::{extract_bool, extract_num, extract_str};
    Some(IterRow {
        iteration: extract_num(line, "iteration")? as u32,
        mode: extract_str(line, "mode").unwrap_or("").to_string(),
        wall_secs: extract_num(line, "wall_secs").unwrap_or(0.0),
        agent_secs: extract_num(line, "agent_secs").unwrap_or(0.0),
        thinking_secs: extract_num(line, "thinking_secs").unwrap_or(0.0),
        guards_secs: extract_num(line, "guards_secs").unwrap_or(0.0),
        num_turns: extract_num(line, "num_turns").unwrap_or(0.0) as u32,
        cost_usd: extract_num(line, "cost_usd").unwrap_or(0.0),
        guards_passed: extract_bool(line, "guards_passed").unwrap_or(false),
        status_done: extract_bool(line, "status_done").unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_stable_across_calls() {
        let a = project_slug();
        let b = project_slug();
        assert_eq!(a, b);
    }

    #[test]
    fn run_id_is_sortable_and_unique() {
        let a = new_run_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_run_id();
        assert_ne!(a, b);
        assert!(b >= a, "run ids should be lexicographically sortable");
    }

    #[test]
    fn tilde_expands() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = expand_tilde("~/foo");
            assert_eq!(p, PathBuf::from(home).join("foo"));
        }
    }

    #[test]
    fn unix_to_utc_known_dates() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(unix_to_utc(1609459200), (2021, 1, 1, 0, 0, 0));
        // 1970-01-01 00:00:00 UTC
        assert_eq!(unix_to_utc(0), (1970, 1, 1, 0, 0, 0));
        // 2024-02-29 12:34:56 UTC (leap day) = 1709210096
        assert_eq!(unix_to_utc(1709210096), (2024, 2, 29, 12, 34, 56));
    }

    #[test]
    fn run_row_round_trips_through_ndjson() {
        let original = RunMetrics {
            run_id: "20260518T000000-000001".to_string(),
            project_slug: "demo-12345678".to_string(),
            mode: "brute",
            started_at: 1700000000,
            ended_at: 1700000050,
            iterations: 3,
            outcome: "judge_pass",
            total_wall_secs: 50.0,
            total_agent_secs: 42.5,
            total_thinking_secs: 4.1,
            total_guards_secs: 3.2,
            total_cost_usd: 0.84,
        };
        let serialized = serialize_row(&original);
        let parsed = parse_run_row(&serialized).expect("parse should succeed");
        assert_eq!(parsed.run_id, original.run_id);
        assert_eq!(parsed.mode, original.mode);
        assert_eq!(parsed.outcome, original.outcome);
        assert_eq!(parsed.iterations, original.iterations);
        assert!((parsed.total_wall_secs - original.total_wall_secs).abs() < 1e-3);
        assert!((parsed.total_cost_usd - original.total_cost_usd).abs() < 1e-3);
    }

    #[test]
    fn iter_row_parses_back() {
        let m = IterationMetrics {
            run_id: "20260518T000000-000001".to_string(),
            project_slug: "demo-12345678".to_string(),
            mode: "loop",
            iteration: 7,
            started_at: 1700000000,
            wall_secs: 12.5,
            restore_ms: 3,
            agent_secs: 10.0,
            guards_secs: 2.0,
            periodics_secs: 0.0,
            hooks_secs: 0.5,
            judge_secs: None,
            agent_reported_secs: Some(9.5),
            cost_usd: 0.13,
            num_turns: 5,
            thinking_secs: 1.2,
            tool_counts: BTreeMap::new(),
            tool_durations_secs: BTreeMap::new(),
            guards: Vec::new(),
            guards_passed: true,
            status_done: false,
        };
        let line = serialize_row(&m);
        let row = parse_iter_row(&line).expect("parse iter row");
        assert_eq!(row.iteration, 7);
        assert_eq!(row.mode, "loop");
        assert_eq!(row.num_turns, 5);
        assert!(row.guards_passed);
        assert!(!row.status_done);
        assert!((row.thinking_secs - 1.2).abs() < 1e-3);
    }

    #[test]
    fn serialize_iteration_is_valid_json_shape() {
        let m = IterationMetrics {
            run_id: "20260101T000000-abcd".to_string(),
            project_slug: "yoke-deadbeef".to_string(),
            mode: "loop",
            iteration: 3,
            started_at: 1700000000,
            wall_secs: 42.5,
            restore_ms: 17,
            agent_secs: 38.0,
            guards_secs: 3.2,
            periodics_secs: 0.0,
            hooks_secs: 0.1,
            judge_secs: None,
            agent_reported_secs: Some(36.7),
            cost_usd: 0.42,
            num_turns: 7,
            thinking_secs: 4.1,
            tool_counts: [("Edit".to_string(), 3u32), ("Bash".to_string(), 1)]
                .into_iter()
                .collect(),
            tool_durations_secs: [("Edit".to_string(), 1.8), ("Bash".to_string(), 12.3)]
                .into_iter()
                .collect(),
            guards: vec![crate::guard::GuardResult {
                name: "cargo test".to_string(),
                passed: true,
                skipped: false,
                elapsed_secs: 2.5,
            }],
            guards_passed: true,
            status_done: false,
        };
        let s = serialize_row(&m);
        assert!(s.starts_with('{') && s.ends_with('}'));
        assert!(s.contains("\"run_id\":\"20260101T000000-abcd\""));
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(parsed["thinking_secs"].as_f64(), Some(4.1));
        assert!(s.contains("\"judge_secs\":null"));
        assert!(s.contains("\"tool_durations_secs\":{"));
    }
}
