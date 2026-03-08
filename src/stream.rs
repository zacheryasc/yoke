use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::ChildStdout;
use std::time::Instant;

use crate::ansi::{BLUE, BOLD, CYAN, DIM, GRAY, GREEN, MAGENTA, ORANGE, RED, RESET, YELLOW};
use crate::json::{extract_num, extract_str, unescape_json};

const BG_RED: &str = "\x1b[48;2;80;30;30m";
const BG_GREEN: &str = "\x1b[48;2;30;60;30m";

struct StreamState {
    turn_num: u32,
    current_msg_id: Option<String>,
    seen_init: bool,
    in_thinking: bool,
    thinking_start: Option<Instant>,
    iteration_cost: f64,
    iteration_duration_secs: f64,
    /// Maps tool_use id → tool name, so tool_result can look up its origin.
    tool_use_names: HashMap<String, String>,
    /// Counts of tool_use events by tool name (for iteration summary strip).
    tool_counts: HashMap<String, u32>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            turn_num: 0,
            current_msg_id: None,
            seen_init: false,
            in_thinking: false,
            thinking_start: None,
            iteration_cost: 0.0,
            iteration_duration_secs: 0.0,
            tool_use_names: HashMap::new(),
            tool_counts: HashMap::new(),
        }
    }
}

/// Render a mini-diff from old_string/new_string extracted from an Edit tool_use.
/// Shows red `−` lines for removed and green `+` lines for added, truncated to ~5 lines.
fn format_edit_diff(line: &str) -> String {
    let old = extract_str(line, "old_string").map(|s| unescape_json(s));
    let new = extract_str(line, "new_string").map(|s| unescape_json(s));

    let (old, new) = match (old, new) {
        (Some(o), Some(n)) => (o, n),
        _ => return String::new(),
    };

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut diff_lines: Vec<String> = Vec::new();
    for ol in &old_lines {
        diff_lines.push(format!("    {}{}{}− {}{}", BG_RED, RED, DIM, ol, RESET));
    }
    for nl in &new_lines {
        diff_lines.push(format!("    {}{}{}+ {}{}", BG_GREEN, GREEN, DIM, nl, RESET));
    }

    let max_display = 5;
    let total = diff_lines.len();
    if total <= max_display {
        diff_lines.join("\n")
    } else {
        let mut out: Vec<String> = diff_lines[..max_display].to_vec();
        out.push(format!("    {}… +{} more lines{}", DIM, total - max_display, RESET));
        out.join("\n")
    }
}

/// Extract the last ~3 lines of error content from a Bash tool_result for display.
fn format_bash_error_tail(line: &str) -> String {
    let content = match extract_str(line, "content") {
        Some(s) => unescape_json(s),
        None => return String::new(),
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let max_tail = 3;
    let start = if lines.len() > max_tail { lines.len() - max_tail } else { 0 };
    let tail: Vec<String> = lines[start..]
        .iter()
        .map(|l| format!("    {}{}{}", RED, l, RESET))
        .collect();
    tail.join("\n")
}

/// Render a preview for Write tool_use: first ~3 lines of content + line count badge.
fn format_write_preview(line: &str) -> String {
    let content = match extract_str(line, "content") {
        Some(s) => unescape_json(s),
        None => return String::new(),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let badge = format!("    {}({} lines){}", DIM, total, RESET);

    let max_preview = 3;
    let preview_lines: Vec<String> = lines.iter()
        .take(max_preview)
        .map(|l| format!("    {}{}{}", DIM, l, RESET))
        .collect();

    let mut out = vec![badge];
    out.extend(preview_lines);
    if total > max_preview {
        out.push(format!("    {}…{}", DIM, RESET));
    }
    out.join("\n")
}

/// Format a badge for Grep/Glob tool_result content.
/// For Grep: tries to count matches/files from the content.
/// For Glob: counts the number of file paths returned.
fn format_grep_glob_badge(tool_name: &str, line: &str) -> String {
    let content = match extract_str(line, "content") {
        Some(s) => unescape_json(s),
        None => return String::new(),
    };

    if content.trim().is_empty() {
        return format!("{}0 results{}", DIM, RESET);
    }

    match tool_name {
        "Grep" => {
            // Grep results are typically one file path per line (files_with_matches mode)
            // or content lines. Count non-empty lines as results.
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            let count = lines.len();
            if count == 1 {
                format!("{} match", count)
            } else {
                format!("{} matches", count)
            }
        }
        "Glob" => {
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            let count = lines.len();
            if count == 1 {
                format!("{} file", count)
            } else {
                format!("{} files", count)
            }
        }
        _ => String::new(),
    }
}

/// Map a file extension to a human-readable language/type label.
fn ext_to_label(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" => Some("javascript"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "jsx" => Some("jsx"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "md" => Some("markdown"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "html" => Some("html"),
        "css" => Some("css"),
        "sql" => Some("sql"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" => Some("c"),
        "cpp" | "cc" | "cxx" => Some("c++"),
        "h" | "hpp" => Some("header"),
        "rb" => Some("ruby"),
        "lua" => Some("lua"),
        "zig" => Some("zig"),
        "lock" => Some("lock"),
        "xml" => Some("xml"),
        "txt" => Some("text"),
        "csv" => Some("csv"),
        "dockerfile" => Some("docker"),
        "tf" => Some("terraform"),
        "ex" | "exs" => Some("elixir"),
        _ => None,
    }
}

/// Format a tool_use event into a human-readable string.
fn format_tool_call(line: &str) -> String {
    let tool_name = extract_str(line, "name").unwrap_or("?");

    match tool_name {
        "Read" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            let badge = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(ext_to_label)
                .map(|label| format!(" {}[{}]{}", DIM, label, RESET))
                .unwrap_or_default();
            format!("{}{}Read:{} {}{}{}{}", BOLD, CYAN, RESET, DIM, path, RESET, badge)
        }
        "Edit" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            let header = format!("{}{}Edit:{} {}{}{}", BOLD, YELLOW, RESET, DIM, path, RESET);
            let diff = format_edit_diff(line);
            if diff.is_empty() {
                header
            } else {
                format!("{}\n{}", header, diff)
            }
        }
        "Write" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            let header = format!("{}{}Write:{} {}{}{}", BOLD, YELLOW, RESET, DIM, path, RESET);
            let preview = format_write_preview(line);
            if preview.is_empty() {
                header
            } else {
                format!("{}\n{}", header, preview)
            }
        }
        "Bash" => {
            let cmd = extract_str(line, "command").unwrap_or("?");
            if cmd.len() > 80 {
                let truncated: String = cmd.chars().take(77).collect();
                format!("{}{}Bash:{} {}{}...{}", BOLD, MAGENTA, RESET, DIM, truncated, RESET)
            } else {
                format!("{}{}Bash:{} {}{}{}", BOLD, MAGENTA, RESET, DIM, cmd, RESET)
            }
        }
        "Glob" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("{}{}Glob:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        "Grep" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("{}{}Grep:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        other => format!("{}{}{}{}", BOLD, BLUE, other, RESET),
    }
}

/// Handle "assistant" events: render tool_use summaries and track tool counts.
fn handle_assistant(out: &mut (impl Write + ?Sized), line: &str, state: &mut StreamState) -> io::Result<()> {
    if !line.contains("\"tool_use\"") {
        return Ok(());
    }
    if let (Some(id), Some(name)) = (extract_str(line, "id"), extract_str(line, "name")) {
        state.tool_use_names.insert(id.to_string(), name.to_string());
        *state.tool_counts.entry(name.to_string()).or_insert(0) += 1;
    }
    let desc = format_tool_call(line);
    writeln!(out, "  {}>>{} {}", GRAY, RESET, desc)
}

/// Handle "stream_event" events: render thinking timer, text deltas, and block boundaries.
fn handle_stream_event(out: &mut (impl Write + ?Sized), line: &str, state: &mut StreamState) -> io::Result<()> {
    if line.contains("\"content_block_delta\"") {
        if line.contains("\"thinking_delta\"") {
            if let Some(start) = state.thinking_start {
                let elapsed = start.elapsed().as_secs_f64();
                write!(out, "\r{}{}thinking {:.1}s{}", DIM, BLUE, elapsed, RESET)?;
                out.flush()?;
            }
        } else if line.contains("\"text_delta\"")
            && let Some(text) = extract_str(line, "text")
        {
            let text = unescape_json(text);
            write!(out, "{}{}{}", DIM, text, RESET)?;
            out.flush()?;
        }
    } else if line.contains("\"content_block_start\"") {
        if line.contains("\"thinking\"") {
            state.thinking_start = Some(Instant::now());
            write!(out, "{}{}thinking 0.0s{}", DIM, BLUE, RESET)?;
            out.flush()?;
            state.in_thinking = true;
        } else if !line.contains("\"tool_use\"") {
            writeln!(out)?;
        }
    } else if line.contains("\"content_block_stop\"") {
        if state.in_thinking {
            if let Some(start) = state.thinking_start {
                let elapsed = start.elapsed().as_secs_f64();
                write!(out, "\r{}{}thinking {:.1}s{}", DIM, BLUE, elapsed, RESET)?;
            }
            state.in_thinking = false;
            state.thinking_start = None;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Handle "user" events: render tool_result success/error badges.
fn handle_tool_result(out: &mut (impl Write + ?Sized), line: &str, state: &StreamState) -> io::Result<()> {
    if !line.contains("\"tool_result\"") {
        return Ok(());
    }
    let is_error = line.contains("\"is_error\":true") || line.contains("\"is_error\": true");
    if is_error {
        writeln!(out, "  {}← {}{}✗{}", GRAY, RESET, RED, RESET)?;
        let tail = format_bash_error_tail(line);
        if !tail.is_empty() {
            writeln!(out, "{}", tail)?;
        }
        return Ok(());
    }
    let tool_name = extract_str(line, "tool_use_id")
        .and_then(|id| state.tool_use_names.get(id))
        .map(|s| s.as_str());
    let badge = match tool_name {
        Some(name @ ("Grep" | "Glob")) => format_grep_glob_badge(name, line),
        _ => String::new(),
    };
    if badge.is_empty() {
        writeln!(out, "  {}← {}✓{}", GRAY, GREEN, RESET)
    } else {
        writeln!(out, "  {}← {}✓{} {}", GRAY, GREEN, RESET, badge)
    }
}

/// Process a single NDJSON line, writing formatted output to `out`.
/// `prior_total` is the accumulated cost from previous iterations, used to display a running total.
/// Returns `Err` on write failure (e.g. broken pipe) so the caller can stop.
fn process_line(out: &mut (impl Write + ?Sized), line: &str, state: &mut StreamState, prior_total: f64) -> io::Result<()> {
    // Check for turn boundary (message_id change)
    if let Some(msg_id) = extract_str(line, "message_id") {
        let is_new = state.current_msg_id.as_deref() != Some(msg_id);
        if is_new {
            state.current_msg_id = Some(msg_id.to_string());
            state.turn_num += 1;
            writeln!(out, "{}{}━━━ Turn {} ━━━{}", BOLD, ORANGE, state.turn_num, RESET)?;
        }
    }

    match extract_str(line, "type") {
        Some("system") => {
            if extract_str(line, "subtype") == Some("init") && !state.seen_init {
                state.seen_init = true;
                let sid = extract_str(line, "session_id").unwrap_or("?");
                let sid_short: String = sid.chars().take(12).collect();
                let model = extract_str(line, "model").unwrap_or("?");
                writeln!(out, "{}{}[stream]{} session {}… model={}", ORANGE, BOLD, RESET, sid_short, model)?;
            }
        }
        Some("assistant") => handle_assistant(out, line, state)?,
        Some("stream_event") => handle_stream_event(out, line, state)?,
        Some("user") => handle_tool_result(out, line, state)?,
        Some("result") => {
            let cost = extract_num(line, "cost_usd").unwrap_or(0.0);
            state.iteration_cost = cost;
            let total = prior_total + cost;
            let turns = extract_num(line, "num_turns").unwrap_or(0.0) as u32;
            let duration = extract_num(line, "duration_ms").unwrap_or(0.0);
            let dur_secs = duration / 1000.0;
            state.iteration_duration_secs = dur_secs;
            writeln!(
                out,
                "{}{}[stream]{} done  cost=${:.2} (total=${:.2})  turns={}  duration={:.1}s",
                ORANGE, BOLD, RESET, cost, total, turns, dur_secs
            )?;
        }
        None if !line.trim().is_empty() => {
            writeln!(out, "{}  {}{}", DIM, line.trim(), RESET)?;
        }
        _ => {}
    }
    Ok(())
}

/// Build a compact one-line iteration summary strip from accumulated state.
/// Format: `⟪ 6 turns │ 3 edits │ 1 bash │ 42s │ $0.38 ⟫`
fn format_summary_strip(state: &StreamState) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Turns
    parts.push(format!("{} turn{}", state.turn_num, if state.turn_num == 1 { "" } else { "s" }));

    // Tool counts — show the most interesting tools in a stable order
    let tool_order = ["Edit", "Write", "Read", "Bash", "Grep", "Glob"];
    for tool in &tool_order {
        if let Some(&count) = state.tool_counts.get(*tool) {
            let label = tool.to_lowercase();
            parts.push(format!("{} {}", count, label));
        }
    }
    // Any tools not in the predefined order
    for (name, &count) in &state.tool_counts {
        if !tool_order.contains(&name.as_str()) {
            parts.push(format!("{} {}", count, name.to_lowercase()));
        }
    }

    // Duration
    parts.push(format!("{:.0}s", state.iteration_duration_secs));

    // Cost
    parts.push(format!("${:.2}", state.iteration_cost));

    format!("{}  ⟪ {} ⟫{}", DIM, parts.join(" │ "), RESET)
}

/// Shared stream loop: reads lines from stdout, tees to log, calls processor per line,
/// prints a summary strip, and returns the iteration cost.
///
/// Used by both Claude and OpenCode stream filters to avoid duplicating the
/// BufReader/signal-check/log-tee boilerplate.
pub fn run_stream_loop<S>(
    stdout: ChildStdout,
    log_path: Option<&Path>,
    state: &mut S,
    mut process: impl FnMut(&mut dyn Write, &str, &mut S) -> io::Result<()>,
    summarize: impl FnOnce(&S) -> Option<String>,
    get_cost: impl FnOnce(&S) -> f64,
) -> f64 {
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

        if let Some(ref mut f) = log_file {
            let _ = writeln!(f, "{}", line);
        }

        if process(&mut out, &line, state).is_err() {
            break;
        }
    }

    if let Some(strip) = summarize(state) {
        let _ = writeln!(out, "{}", strip);
    }

    let _ = out.flush();
    get_cost(state)
}

/// Filter NDJSON stream from Claude and format as rich ANSI output on stdout.
/// `prior_total` is the accumulated cost from previous iterations.
/// Returns the cost of this iteration (from the `result` event).
pub fn filter_stream(stdout: ChildStdout, log_path: Option<&Path>, prior_total: f64) -> f64 {
    let mut state = StreamState::new();
    run_stream_loop(
        stdout,
        log_path,
        &mut state,
        |out, line, st| process_line(out, line, st, prior_total),
        |st| if st.turn_num > 0 { Some(format_summary_strip(st)) } else { None },
        |st| st.iteration_cost,
    )
}
