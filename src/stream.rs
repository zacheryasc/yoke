use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::ChildStdout;

use crate::json::{extract_num, extract_str};

// ANSI escape codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";

struct StreamState {
    turn_num: u32,
    current_msg_id: Option<String>,
    seen_init: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            turn_num: 0,
            current_msg_id: None,
            seen_init: false,
        }
    }
}

/// Format a tool_use event into a human-readable string.
fn format_tool_call(line: &str) -> String {
    let tool_name = extract_str(line, "name").unwrap_or("?");

    match tool_name {
        "Read" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            format!("Read: {}", path)
        }
        "Edit" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            format!("Edit: {}", path)
        }
        "Write" => {
            let path = extract_str(line, "file_path").unwrap_or("?");
            format!("Write: {}", path)
        }
        "Bash" => {
            let cmd = extract_str(line, "command").unwrap_or("?");
            if cmd.len() > 80 {
                format!("Bash: {}...", &cmd[..77])
            } else {
                format!("Bash: {}", cmd)
            }
        }
        "Glob" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("Glob: {}", pat)
        }
        "Grep" => {
            let pat = extract_str(line, "pattern").unwrap_or("?");
            format!("Grep: {}", pat)
        }
        other => other.to_string(),
    }
}

/// Filter NDJSON stream from Claude and format as rich ANSI output on stdout.
/// Consumes the stream entirely — raw NDJSON is not written to disk.
pub fn filter_stream(stdout: ChildStdout, log_path: Option<&Path>) {
    let reader = BufReader::new(stdout);
    let mut state = StreamState::new();
    let mut log_file = log_path.and_then(|p| {
        std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).ok();
        std::fs::File::create(p).ok()
    });

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

        // Tee raw NDJSON to log file
        if let Some(ref mut f) = log_file {
            let _ = writeln!(f, "{}", line);
        }

        // Check for turn boundary (message_id change)
        if let Some(msg_id) = extract_str(&line, "message_id") {
            let changed = match &state.current_msg_id {
                Some(prev) => prev != msg_id,
                None => true,
            };
            if changed {
                state.current_msg_id = Some(msg_id.to_string());
                state.turn_num += 1;
                println!(
                    "{}{}━━━ Turn {} ━━━{}",
                    BOLD, CYAN, state.turn_num, RESET
                );
            }
        }

        let ev_type = extract_str(&line, "type");

        match ev_type {
            // system → check subtype for init
            Some("system") => {
                let ev_subtype = extract_str(&line, "subtype");
                if ev_subtype == Some("init") && !state.seen_init {
                    state.seen_init = true;
                    let sid = extract_str(&line, "session_id").unwrap_or("?");
                    let sid_short = if sid.len() > 12 { &sid[..12] } else { sid };
                    let model = extract_str(&line, "model").unwrap_or("?");
                    println!(
                        "{}{}[stream] session {}... model={}{}",
                        CYAN, BOLD, sid_short, model, RESET
                    );
                }
            }
            // assistant → probe content for tool_use or text
            Some("assistant") => {
                if line.contains("\"tool_use\"") {
                    let desc = format_tool_call(&line);
                    println!("{}{}>>{} {}{}", YELLOW, BOLD, RESET, desc, RESET);
                } else if let Some(text) = extract_str(&line, "text") {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        for text_line in trimmed.lines().take(3) {
                            let truncated = if text_line.len() > 120 {
                                &text_line[..120]
                            } else {
                                text_line
                            };
                            println!("{}  {}{}", DIM, truncated, RESET);
                        }
                    }
                }
            }
            // result → green bold summary
            Some("result") => {
                let cost = extract_num(&line, "cost_usd").unwrap_or(0.0);
                let turns = extract_num(&line, "num_turns").unwrap_or(0.0) as u32;
                let duration = extract_num(&line, "duration_ms").unwrap_or(0.0);
                let dur_secs = duration / 1000.0;
                println!(
                    "{}{}[stream] done  cost=${:.2} turns={} duration={:.1}s{}",
                    GREEN, BOLD, cost, turns, dur_secs, RESET
                );
            }
            // Non-JSON or unrecognized — dim passthrough
            _ => {
                if ev_type.is_none() && !line.trim().is_empty() {
                    println!("{}  {}{}", DIM, line.trim(), RESET);
                }
            }
        }
    }
}
