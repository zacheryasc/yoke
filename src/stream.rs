use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::ChildStdout;

/// Aggregated, persistable view of one agent invocation's stream.
///
/// Produced after the child's stdout closes. Carries everything the metrics
/// layer wants: cost, agent-reported wall time, thinking total, and per-tool
/// durations + counts.
#[derive(Debug, Clone, Default)]
pub struct StreamSummary {
    pub cost_usd: f64,
    /// Wall clock as reported by the agent's stream, when available.
    pub agent_reported_secs: Option<f64>,
    pub num_turns: u32,
    pub thinking_secs: f64,
    pub tool_counts: BTreeMap<String, u32>,
    pub tool_durations_secs: BTreeMap<String, f64>,
    /// OMP session ID extracted from the `session` event. Used by the harness
    /// to resume the same worker session on the next iteration.
    pub session_id: Option<String>,
}

/// Shared stream loop: reads lines from stdout, tees to log, calls processor per
/// line, prints a summary strip, and finalizes state into a caller-defined
/// return type.
pub fn run_stream_loop<S, R>(
    stdout: ChildStdout,
    log_path: Option<&Path>,
    mut state: S,
    mut process: impl FnMut(&mut dyn Write, &str, &mut S) -> io::Result<()>,
    summarize: impl FnOnce(&S) -> Option<String>,
    finalize: impl FnOnce(S) -> R,
) -> R {
    let reader = BufReader::new(stdout);
    let mut log_file = log_path.and_then(|p| {
        std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).ok();
        std::fs::File::create(p).ok()
    });

    let mut out = io::stdout().lock();

    for line_result in reader.lines() {
        if crate::signal::interrupted() {
            break;
        }

        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            continue;
        }

        if let Some(f) = &mut log_file {
            let _ = writeln!(f, "{}", line);
        }

        if process(&mut out, &line, &mut state).is_err() {
            break;
        }
    }

    if let Some(strip) = summarize(&state) {
        let _ = writeln!(out, "{}", strip);
    }

    let _ = out.flush();
    finalize(state)
}
