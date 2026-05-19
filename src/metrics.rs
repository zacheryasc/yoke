//! Persistent metrics for yoke runs.  Rows are written under `~/.yoke/metrics/`
//! (or the configured `metrics-dir`) as NDJSON — one row per iteration, one
//! row per completed run.  Storage lives outside the project tree so it
//! survives `yoke clean`, `yoke stash`, branch resets, and project deletes.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct GuardRow {
    pub name: String,
    pub passed: bool,
    pub skipped: bool,
    pub elapsed_secs: f64,
}

#[derive(Debug, Clone)]
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

    pub guards: Vec<GuardRow>,

    pub guards_passed: bool,
    pub status_done: bool,
}

#[derive(Debug, Clone)]
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

/// Run-scoped state that travels with the loop: identifies the run, knows
/// where to write, and tells the loop whether persistence is enabled.
#[derive(Debug, Clone)]
pub struct MetricsContext {
    pub run_id: String,
    pub project_slug: String,
    pub mode: &'static str,
    pub root_dir: PathBuf,
    pub enabled: bool,
}

impl MetricsContext {
    pub fn new(root_dir: PathBuf, mode: &'static str, enabled: bool) -> Self {
        Self {
            run_id: new_run_id(),
            project_slug: project_slug(),
            mode,
            root_dir,
            enabled,
        }
    }

    pub fn iteration_path(&self) -> PathBuf {
        self.root_dir
            .join(&self.project_slug)
            .join(format!("{}.ndjson", self.run_id))
    }

    pub fn runs_path(&self) -> PathBuf {
        self.root_dir.join(&self.project_slug).join("runs.ndjson")
    }
}

/// Accumulates per-iteration totals and writes a final `RunMetrics` row
/// on Drop.  Drop runs on normal exit, on early-return, and on the first
/// SIGINT (which sets the interrupted flag and lets the loop unwind).  A
/// second SIGINT calls `_exit` directly and bypasses Drop — accepted
/// tradeoff for a panic-button.
pub struct RunRecorder {
    pub ctx: MetricsContext,
    started_at: u64,
    wall_start: std::time::Instant,
    iterations: u32,
    total_agent_secs: f64,
    total_thinking_secs: f64,
    total_guards_secs: f64,
    total_cost_usd: f64,
    outcome: &'static str,
    /// Closure invoked with the finalized RunMetrics from Drop. Allows the
    /// caller to render an end-of-run summary table without forcing Drop
    /// to know about ANSI rendering or print to stderr unconditionally.
    on_finalize: Option<Box<dyn FnMut(&RunMetrics) + Send>>,
}

impl RunRecorder {
    pub fn new(ctx: MetricsContext) -> Self {
        Self {
            ctx,
            started_at: unix_now(),
            wall_start: std::time::Instant::now(),
            iterations: 0,
            total_agent_secs: 0.0,
            total_thinking_secs: 0.0,
            total_guards_secs: 0.0,
            total_cost_usd: 0.0,
            // Default to "interrupt" so an unwinding stack still records
            // something meaningful — clean exit paths override this.
            outcome: "interrupt",
            on_finalize: None,
        }
    }

    pub fn on_finalize<F>(&mut self, cb: F)
    where
        F: FnMut(&RunMetrics) + Send + 'static,
    {
        self.on_finalize = Some(Box::new(cb));
    }

    /// Write the iteration row and accumulate its totals in one step.
    pub fn record_iteration(&mut self, m: &IterationMetrics) {
        write_iteration_row(&self.ctx, m);
        self.iterations += 1;
        self.total_agent_secs += m.agent_secs;
        self.total_thinking_secs += m.thinking_secs;
        self.total_guards_secs += m.guards_secs;
        self.total_cost_usd += m.cost_usd;
    }

    pub fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    fn finalize(&mut self) -> RunMetrics {
        RunMetrics {
            run_id: self.ctx.run_id.clone(),
            project_slug: self.ctx.project_slug.clone(),
            mode: self.ctx.mode,
            started_at: self.started_at,
            ended_at: unix_now(),
            iterations: self.iterations,
            outcome: self.outcome,
            total_wall_secs: self.wall_start.elapsed().as_secs_f64(),
            total_agent_secs: self.total_agent_secs,
            total_thinking_secs: self.total_thinking_secs,
            total_guards_secs: self.total_guards_secs,
            total_cost_usd: self.total_cost_usd,
        }
    }
}

impl Drop for RunRecorder {
    fn drop(&mut self) {
        let run = self.finalize();
        write_run_row(&self.ctx, &run);
        if let Some(mut cb) = self.on_finalize.take() {
            cb(&run);
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
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
fn unix_to_utc(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let hh = (tod / 3600) as u32;
    let mm = ((tod % 3600) / 60) as u32;
    let ss = (tod % 60) as u32;

    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
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
    let abs = cwd.canonicalize().unwrap_or(cwd.clone());
    let base = abs
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let hash = fnv1a_32(abs.to_string_lossy().as_bytes());
    format!("{}-{:08x}", sanitize(base), hash)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
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

pub fn write_iteration_row(ctx: &MetricsContext, m: &IterationMetrics) {
    if !ctx.enabled {
        return;
    }
    append_line(&ctx.iteration_path(), &serialize_iteration(m));
}

pub fn write_run_row(ctx: &MetricsContext, r: &RunMetrics) {
    if !ctx.enabled {
        return;
    }
    append_line(&ctx.runs_path(), &serialize_run(r));
}

fn append_line(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "[yoke] WARNING: cannot create metrics dir {}: {}",
                parent.display(),
                e
            );
            return;
        }
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            // Single write_all keeps the row atomic on POSIX for small lines.
            let mut buf = line.to_string();
            buf.push('\n');
            if let Err(e) = f.write_all(buf.as_bytes()) {
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

fn serialize_iteration(m: &IterationMetrics) -> String {
    let mut s = String::with_capacity(512);
    s.push('{');
    push_str_field(&mut s, "run_id", &m.run_id, true);
    push_str_field(&mut s, "project_slug", &m.project_slug, false);
    push_str_field(&mut s, "mode", m.mode, false);
    push_u32_field(&mut s, "iteration", m.iteration, false);
    push_u64_field(&mut s, "started_at", m.started_at, false);
    push_f64_field(&mut s, "wall_secs", m.wall_secs, false);
    push_u64_field(&mut s, "restore_ms", m.restore_ms, false);
    push_f64_field(&mut s, "agent_secs", m.agent_secs, false);
    push_f64_field(&mut s, "guards_secs", m.guards_secs, false);
    push_f64_field(&mut s, "periodics_secs", m.periodics_secs, false);
    push_f64_field(&mut s, "hooks_secs", m.hooks_secs, false);
    push_opt_f64_field(&mut s, "judge_secs", m.judge_secs);
    push_opt_f64_field(&mut s, "agent_reported_secs", m.agent_reported_secs);
    push_f64_field(&mut s, "cost_usd", m.cost_usd, false);
    push_u32_field(&mut s, "num_turns", m.num_turns, false);
    push_f64_field(&mut s, "thinking_secs", m.thinking_secs, false);
    s.push_str(",\"tool_counts\":");
    push_map_u32(&mut s, &m.tool_counts);
    s.push_str(",\"tool_durations_secs\":");
    push_map_f64(&mut s, &m.tool_durations_secs);
    s.push_str(",\"guards\":[");
    for (i, g) in m.guards.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('{');
        push_str_field(&mut s, "name", &g.name, true);
        push_bool_field(&mut s, "passed", g.passed, false);
        push_bool_field(&mut s, "skipped", g.skipped, false);
        push_f64_field(&mut s, "elapsed_secs", g.elapsed_secs, false);
        s.push('}');
    }
    s.push(']');
    push_bool_field(&mut s, "guards_passed", m.guards_passed, false);
    push_bool_field(&mut s, "status_done", m.status_done, false);
    s.push('}');
    s
}

fn serialize_run(r: &RunMetrics) -> String {
    let mut s = String::with_capacity(256);
    s.push('{');
    push_str_field(&mut s, "run_id", &r.run_id, true);
    push_str_field(&mut s, "project_slug", &r.project_slug, false);
    push_str_field(&mut s, "mode", r.mode, false);
    push_u64_field(&mut s, "started_at", r.started_at, false);
    push_u64_field(&mut s, "ended_at", r.ended_at, false);
    push_u32_field(&mut s, "iterations", r.iterations, false);
    push_str_field(&mut s, "outcome", r.outcome, false);
    push_f64_field(&mut s, "total_wall_secs", r.total_wall_secs, false);
    push_f64_field(&mut s, "total_agent_secs", r.total_agent_secs, false);
    push_f64_field(&mut s, "total_thinking_secs", r.total_thinking_secs, false);
    push_f64_field(&mut s, "total_guards_secs", r.total_guards_secs, false);
    push_f64_field(&mut s, "total_cost_usd", r.total_cost_usd, false);
    s.push('}');
    s
}

fn push_str_field(s: &mut String, k: &str, v: &str, first: bool) {
    if !first {
        s.push(',');
    }
    s.push('"');
    s.push_str(k);
    s.push_str("\":\"");
    escape_str_into(s, v);
    s.push('"');
}

fn push_u32_field(s: &mut String, k: &str, v: u32, first: bool) {
    if !first {
        s.push(',');
    }
    s.push('"');
    s.push_str(k);
    s.push_str("\":");
    s.push_str(&v.to_string());
}

fn push_u64_field(s: &mut String, k: &str, v: u64, first: bool) {
    if !first {
        s.push(',');
    }
    s.push('"');
    s.push_str(k);
    s.push_str("\":");
    s.push_str(&v.to_string());
}

fn push_f64_field(s: &mut String, k: &str, v: f64, first: bool) {
    if !first {
        s.push(',');
    }
    s.push('"');
    s.push_str(k);
    s.push_str("\":");
    push_f64_value(s, v);
}

fn push_opt_f64_field(s: &mut String, k: &str, v: Option<f64>) {
    s.push(',');
    s.push('"');
    s.push_str(k);
    s.push_str("\":");
    match v {
        Some(x) => push_f64_value(s, x),
        None => s.push_str("null"),
    }
}

fn push_f64_value(s: &mut String, v: f64) {
    if v.is_finite() {
        // 4 dp is more than enough for second-scale measurements
        s.push_str(&format!("{:.4}", v));
    } else {
        s.push_str("null");
    }
}

fn push_bool_field(s: &mut String, k: &str, v: bool, first: bool) {
    if !first {
        s.push(',');
    }
    s.push('"');
    s.push_str(k);
    s.push_str("\":");
    s.push_str(if v { "true" } else { "false" });
}

fn escape_str_into(s: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
}

fn push_map_u32(s: &mut String, m: &BTreeMap<String, u32>) {
    s.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        escape_str_into(s, k);
        s.push_str("\":");
        s.push_str(&v.to_string());
    }
    s.push('}');
}

fn push_map_f64(s: &mut String, m: &BTreeMap<String, f64>) {
    s.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        escape_str_into(s, k);
        s.push_str("\":");
        push_f64_value(s, *v);
    }
    s.push('}');
}

// ─── Read side: parse rows for `yoke stats` ──────────────────────────────
//
// The write side uses `&'static str` for mode/outcome — small win on the
// hot path.  The read side comes from runtime data so it uses owned
// `String` fields and a separate row type.  Keeps both sides honest
// without forcing one to bend to the other.

/// One parsed row from `runs.ndjson`.  Some fields aren't shown in the
/// current `yoke stats` output but are populated for callers that want
/// to filter or aggregate.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub project_slug: String,
    pub mode: String,
    pub started_at: u64,
    pub ended_at: u64,
    pub iterations: u32,
    pub outcome: String,
    pub total_wall_secs: f64,
    pub total_agent_secs: f64,
    pub total_thinking_secs: f64,
    pub total_guards_secs: f64,
    pub total_cost_usd: f64,
}

/// One parsed row from `<run-id>.ndjson`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct IterRow {
    pub iteration: u32,
    pub mode: String,
    pub started_at: u64,
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
    let path = slug_dir.join("runs.ndjson");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(r) = parse_run_row(line) {
            out.push(r);
        }
    }
    out
}

pub fn read_iterations(file: &Path) -> Vec<IterRow> {
    let content = match fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(r) = parse_iter_row(line) {
            out.push(r);
        }
    }
    out
}

fn parse_run_row(line: &str) -> Option<RunRow> {
    use crate::json::{extract_num, extract_str};
    Some(RunRow {
        run_id: extract_str(line, "run_id")?.to_string(),
        project_slug: extract_str(line, "project_slug")?.to_string(),
        mode: extract_str(line, "mode")?.to_string(),
        started_at: extract_num(line, "started_at")? as u64,
        ended_at: extract_num(line, "ended_at")? as u64,
        iterations: extract_num(line, "iterations")? as u32,
        outcome: extract_str(line, "outcome")?.to_string(),
        total_wall_secs: extract_num(line, "total_wall_secs").unwrap_or(0.0),
        total_agent_secs: extract_num(line, "total_agent_secs").unwrap_or(0.0),
        total_thinking_secs: extract_num(line, "total_thinking_secs").unwrap_or(0.0),
        total_guards_secs: extract_num(line, "total_guards_secs").unwrap_or(0.0),
        total_cost_usd: extract_num(line, "total_cost_usd").unwrap_or(0.0),
    })
}

fn parse_iter_row(line: &str) -> Option<IterRow> {
    use crate::json::{extract_bool, extract_num, extract_str};
    Some(IterRow {
        iteration: extract_num(line, "iteration")? as u32,
        mode: extract_str(line, "mode").unwrap_or("").to_string(),
        started_at: extract_num(line, "started_at").unwrap_or(0.0) as u64,
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
        let serialized = serialize_run(&original);
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
        let line = serialize_iteration(&m);
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
            tool_counts: [("Edit".to_string(), 3u32), ("Bash".to_string(), 1)].into_iter().collect(),
            tool_durations_secs: [("Edit".to_string(), 1.8), ("Bash".to_string(), 12.3)].into_iter().collect(),
            guards: vec![GuardRow {
                name: "cargo test".to_string(),
                passed: true,
                skipped: false,
                elapsed_secs: 2.5,
            }],
            guards_passed: true,
            status_done: false,
        };
        let s = serialize_iteration(&m);
        assert!(s.starts_with('{') && s.ends_with('}'));
        assert!(s.contains("\"run_id\":\"20260101T000000-abcd\""));
        assert!(s.contains("\"thinking_secs\":4.1000"));
        assert!(s.contains("\"judge_secs\":null"));
        assert!(s.contains("\"tool_durations_secs\":{"));
    }
}
