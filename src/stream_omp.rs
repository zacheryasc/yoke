use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::ChildStdout;
use std::time::Instant;

use crate::ansi::{BLUE, BOLD, CYAN, DIM, GRAY, GREEN, MAGENTA, ORANGE, RED, RESET, YELLOW};
use crate::json::{extract_bool, extract_num, extract_str, unescape_json};
use crate::stream::StreamSummary;

struct StreamState {
    turn_num: u32,
    iteration_cost: f64,
    iteration_duration_secs: f64,
    tool_counts: HashMap<String, u32>,
    tool_durations: HashMap<String, f64>,
    tool_starts: HashMap<String, (String, Instant)>,
    saw_done: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            turn_num: 0,
            iteration_cost: 0.0,
            iteration_duration_secs: 0.0,
            tool_counts: HashMap::new(),
            tool_durations: HashMap::new(),
            tool_starts: HashMap::new(),
            saw_done: false,
        }
    }

    fn into_summary(self) -> StreamSummary {
        StreamSummary {
            cost_usd: self.iteration_cost,
            agent_reported_secs: if self.iteration_duration_secs > 0.0 {
                Some(self.iteration_duration_secs)
            } else {
                None
            },
            num_turns: self.turn_num,
            thinking_secs: 0.0,
            tool_counts: self.tool_counts.into_iter().collect(),
            tool_durations_secs: self.tool_durations.into_iter().collect(),
        }
    }
}

fn format_tool_call(tool_name: &str, line: &str) -> String {
    match tool_name {
        "read" => {
            let path = extract_str(line, "path")
                .or_else(|| extract_str(line, "filePath"))
                .unwrap_or("?");
            format!("{}{}Read:{} {}{}{}", BOLD, CYAN, RESET, DIM, path, RESET)
        }
        "write" => {
            let path = extract_str(line, "path")
                .or_else(|| extract_str(line, "filePath"))
                .unwrap_or("?");
            format!("{}{}Write:{} {}{}{}", BOLD, YELLOW, RESET, DIM, path, RESET)
        }
        "edit" | "apply_patch" => {
            format!("{}{}Edit{}{}", BOLD, YELLOW, RESET, RESET)
        }
        "bash" => {
            let cmd = extract_str(line, "command").unwrap_or("?");
            let cmd = unescape_json(cmd);
            let mut chars = cmd.chars();
            let preview: String = chars.by_ref().take(77).collect();
            if chars.next().is_some() {
                format!(
                    "{}{}Bash:{} {}{}...{}",
                    BOLD, MAGENTA, RESET, DIM, preview, RESET
                )
            } else {
                format!(
                    "{}{}Bash:{} {}{}{}",
                    BOLD, MAGENTA, RESET, DIM, preview, RESET
                )
            }
        }
        "glob" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("{}{}Glob:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        "grep" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("{}{}Grep:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        other => format!("{}{}{}{}", BOLD, BLUE, other, RESET),
    }
}

fn handle_tool_start(
    out: &mut (impl Write + ?Sized),
    line: &str,
    state: &mut StreamState,
) -> io::Result<()> {
    let tool_name = extract_str(line, "toolName").unwrap_or("?");
    let tool_id = extract_str(line, "toolCallId").unwrap_or("");
    *state.tool_counts.entry(tool_name.to_string()).or_insert(0) += 1;
    if !tool_id.is_empty() {
        state
            .tool_starts
            .insert(tool_id.to_string(), (tool_name.to_string(), Instant::now()));
    }
    let desc = format_tool_call(tool_name, line);
    writeln!(out, "  {}>>{} {}", GRAY, RESET, desc)
}

fn handle_tool_end(
    out: &mut (impl Write + ?Sized),
    line: &str,
    state: &mut StreamState,
) -> io::Result<()> {
    let tool_name = extract_str(line, "toolName").unwrap_or("?").to_string();
    let elapsed = extract_num(line, "wallTimeMs")
        .map(|ms| ms / 1000.0)
        .or_else(|| {
            extract_str(line, "toolCallId").and_then(|id| {
                state
                    .tool_starts
                    .remove(id)
                    .map(|(_, start)| start.elapsed().as_secs_f64())
            })
        });
    if let Some(secs) = elapsed {
        *state.tool_durations.entry(tool_name).or_insert(0.0) += secs;
    }

    let is_error = extract_bool(line, "isError").unwrap_or(false);
    if is_error {
        writeln!(out, "  {}← {}{}✗{}", GRAY, RESET, RED, RESET)
    } else {
        writeln!(out, "  {}← {}✓{}", GRAY, GREEN, RESET)
    }
}

fn handle_message_end(line: &str, state: &mut StreamState) {
    if extract_str(line, "role") != Some("assistant") {
        return;
    }
    state.iteration_cost += extract_num(line, "total").unwrap_or(0.0);
    state.iteration_duration_secs += extract_num(line, "duration").unwrap_or(0.0) / 1000.0;
}

fn print_done(
    out: &mut (impl Write + ?Sized),
    state: &mut StreamState,
    prior_total: f64,
) -> io::Result<()> {
    if state.saw_done {
        return Ok(());
    }
    state.saw_done = true;
    writeln!(
        out,
        "{}{}[stream]{} done  cost=${:.2} (total=${:.2})  turns={}  duration={:.1}s",
        ORANGE,
        BOLD,
        RESET,
        state.iteration_cost,
        prior_total + state.iteration_cost,
        state.turn_num,
        state.iteration_duration_secs
    )
}

fn process_line(
    out: &mut (impl Write + ?Sized),
    line: &str,
    state: &mut StreamState,
    prior_total: f64,
) -> io::Result<()> {
    match extract_str(line, "type") {
        Some("session") => {
            if let Some(id) = extract_str(line, "id") {
                let sid_short: String = id.chars().take(12).collect();
                writeln!(
                    out,
                    "{}{}[stream]{} session {}…",
                    ORANGE, BOLD, RESET, sid_short
                )?;
            }
        }
        Some("turn_start") => {
            state.turn_num += 1;
            writeln!(
                out,
                "{}{}━━━ Turn {} ━━━{}",
                BOLD, ORANGE, state.turn_num, RESET
            )?;
        }
        Some("message_update") => {
            if line.contains("\"type\":\"text_delta\"") || line.contains("\"type\": \"text_delta\"")
            {
                if let Some(delta) = extract_str(line, "delta") {
                    write!(out, "{}{}{}", DIM, unescape_json(delta), RESET)?;
                    out.flush()?;
                }
            }
        }
        Some("tool_execution_start") => handle_tool_start(out, line, state)?,
        Some("tool_execution_end") => handle_tool_end(out, line, state)?,
        Some("message_end") => handle_message_end(line, state),
        Some("agent_end") => print_done(out, state, prior_total)?,
        None if !line.trim().is_empty() => {
            writeln!(out, "{}  {}{}", DIM, line.trim(), RESET)?;
        }
        _ => {}
    }
    Ok(())
}

fn format_summary_strip(state: &StreamState) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "{} turn{}",
        state.turn_num,
        if state.turn_num == 1 { "" } else { "s" }
    ));

    let mut ranked: Vec<(&String, u32, f64)> = state
        .tool_counts
        .iter()
        .map(|(name, &count)| {
            let secs = state.tool_durations.get(name).copied().unwrap_or(0.0);
            (name, count, secs)
        })
        .collect();
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    for (i, (name, count, secs)) in ranked.iter().enumerate() {
        if i < 3 && *secs > 0.0 {
            parts.push(format!("{} {} {:.1}s", count, name, secs));
        } else {
            parts.push(format!("{} {}", count, name));
        }
    }

    parts.push(format!("{:.0}s", state.iteration_duration_secs));
    parts.push(format!("${:.2}", state.iteration_cost));

    format!("{}  ⟪ {} ⟫{}", DIM, parts.join(" │ "), RESET)
}

/// Incremental OMP NDJSON formatter for runtimes that do not expose a
/// `ChildStdout` directly, such as OMP RPC.
pub(crate) struct OmpStreamProcessor {
    state: StreamState,
    log_file: Option<std::fs::File>,
    prior_total: f64,
}

impl OmpStreamProcessor {
    pub(crate) fn new(log_path: Option<&Path>, prior_total: f64) -> Self {
        let log_file = log_path.and_then(|p| {
            std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).ok();
            std::fs::File::create(p).ok()
        });
        Self {
            state: StreamState::new(),
            log_file,
            prior_total,
        }
    }

    /// Process one raw OMP JSONL line. Returns true when the line ends the
    /// current agent turn.
    pub(crate) fn process_line(&mut self, line: &str) -> bool {
        if line.trim().is_empty() {
            return false;
        }
        if let Some(f) = &mut self.log_file {
            let _ = writeln!(f, "{}", line);
        }
        let mut out = io::stdout().lock();
        let _ = process_line(&mut out, line, &mut self.state, self.prior_total);
        let _ = out.flush();
        extract_str(line, "type") == Some("agent_end")
    }

    pub(crate) fn finish(self) -> StreamSummary {
        let mut out = io::stdout().lock();
        if self.state.turn_num > 0 {
            let _ = writeln!(out, "{}", format_summary_strip(&self.state));
        }
        let _ = out.flush();
        self.state.into_summary()
    }
}

/// Filter OMP NDJSON and format it as rich ANSI output on stdout.
pub fn filter_stream(
    stdout: ChildStdout,
    log_path: Option<&Path>,
    prior_total: f64,
) -> StreamSummary {
    let mut state = StreamState::new();
    let mut log_file = log_path.and_then(|p| {
        std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).ok();
        std::fs::File::create(p).ok()
    });
    let mut out = io::stdout().lock();
    for line_result in BufReader::new(stdout).lines() {
        if crate::signal::interrupted() {
            break;
        }
        let Ok(line) = line_result else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(file) = &mut log_file {
            let _ = writeln!(file, "{}", line);
        }
        if process_line(&mut out, &line, &mut state, prior_total).is_err() {
            break;
        }
    }
    if state.turn_num > 0 {
        let _ = writeln!(out, "{}", format_summary_strip(&state));
    }
    let _ = out.flush();
    state.into_summary()
}
