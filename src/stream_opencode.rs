use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::ChildStdout;

use crate::ansi::{BLUE, BOLD, CYAN, DIM, GRAY, GREEN, MAGENTA, ORANGE, RED, RESET, YELLOW};
use crate::json::{extract_num, extract_str, unescape_json};

struct StreamState {
    turn_num: u32,
    iteration_cost: f64,
    tool_counts: HashMap<String, u32>,
    total_tokens: u64,
}

impl StreamState {
    fn new() -> Self {
        Self {
            turn_num: 0,
            iteration_cost: 0.0,
            tool_counts: HashMap::new(),
            total_tokens: 0,
        }
    }
}

fn format_tool_call(tool_name: &str, input: &str) -> String {
    match tool_name {
        "read" => {
            let path = extract_str(input, "filePath").unwrap_or("?");
            format!("{}{}Read:{} {}{}{}", BOLD, CYAN, RESET, DIM, path, RESET)
        }
        "write" => {
            let path = extract_str(input, "filePath").unwrap_or("?");
            format!("{}{}Write:{} {}{}{}", BOLD, YELLOW, RESET, DIM, path, RESET)
        }
        "apply_patch" => {
            format!("{}{}ApplyPatch{}{}", BOLD, YELLOW, RESET, RESET)
        }
        "bash" => {
            let cmd = extract_str(input, "command").unwrap_or("?");
            if cmd.len() > 80 {
                format!(
                    "{}{}Bash:{} {}{}...{}",
                    BOLD,
                    MAGENTA,
                    RESET,
                    DIM,
                    &cmd.chars().take(77).collect::<String>(),
                    RESET
                )
            } else {
                format!("{}{}Bash:{} {}{}{}", BOLD, MAGENTA, RESET, DIM, cmd, RESET)
            }
        }
        "glob" => {
            let pat = extract_str(input, "pattern").unwrap_or("?");
            format!("{}{}Glob:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        "grep" => {
            let pat = extract_str(input, "pattern").unwrap_or("?");
            format!("{}{}Grep:{} {}{}{}", BOLD, CYAN, RESET, DIM, pat, RESET)
        }
        other => format!("{}{}{}{}", BOLD, BLUE, other, RESET),
    }
}

fn process_line(out: &mut (impl Write + ?Sized), line: &str, state: &mut StreamState) -> io::Result<()> {
    let ev_type = extract_str(line, "type");

    match ev_type {
        Some("step_start") => {
            state.turn_num += 1;
            writeln!(
                out,
                "{}{}━━━ Turn {} ━━━{}",
                BOLD, ORANGE, state.turn_num, RESET
            )?;
        }
        Some("text") => {
            if let Some(text) = extract_str(line, "text") {
                let text = unescape_json(text);
                write!(out, "{}{}{}", DIM, text, RESET)?;
                out.flush()?;
            }
        }
        Some("tool_use") => {
            let tool_name = extract_str(line, "tool").unwrap_or("?");

            let status = if line.contains("\"status\":\"error\"")
                || line.contains("\"status\": \"error\"")
            {
                "error"
            } else if line.contains("\"status\":\"completed\"")
                || line.contains("\"status\": \"completed\"")
            {
                "completed"
            } else {
                "pending"
            };

            *state.tool_counts.entry(tool_name.to_string()).or_insert(0) += 1;

            if status == "error" {
                if let Some(error) = extract_str(line, "error") {
                    let error = unescape_json(error);
                    writeln!(
                        out,
                        "  {}>> {}{}{} {}{}✗{}",
                        GRAY, RESET, BOLD, tool_name, RESET, RED, RESET
                    )?;
                    writeln!(out, "    {}{}{}", RED, error, RESET)?;
                } else {
                    writeln!(
                        out,
                        "  {}>> {}{}{} {}{}✗{}",
                        GRAY, RESET, BOLD, tool_name, RESET, RED, RESET
                    )?;
                }
            } else if status == "completed" {
                let input = extract_str(line, "input").unwrap_or("");
                let desc = format_tool_call(tool_name, input);
                writeln!(out, "  {}>>{} {}", GRAY, RESET, desc)?;
                writeln!(out, "  {}← {}✓{}", GRAY, GREEN, RESET)?;
            } else {
                let input = extract_str(line, "input").unwrap_or("");
                let desc = format_tool_call(tool_name, input);
                writeln!(out, "  {}>>{} {}", GRAY, RESET, desc)?;
            }
        }
        Some("step_finish") => {
            let cost = extract_num(line, "cost").unwrap_or(0.0);
            state.iteration_cost += cost;

            let tokens = extract_num(line, "total").unwrap_or(0.0) as u64;
            state.total_tokens = tokens;
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

    let tool_order = ["bash", "read", "write", "apply_patch", "glob", "grep"];
    for tool in &tool_order {
        if let Some(&count) = state.tool_counts.get(*tool) {
            parts.push(format!("{} {}", count, tool));
        }
    }
    for (name, &count) in &state.tool_counts {
        if !tool_order.contains(&name.as_str()) {
            parts.push(format!("{} {}", count, name));
        }
    }

    parts.push(format!("${:.2}", state.iteration_cost));

    format!("{}  ⟪ {} ⟫{}", DIM, parts.join(" │ "), RESET)
}

pub fn filter_stream(stdout: ChildStdout, log_path: Option<&Path>, _prior_total: f64) -> f64 {
    let mut state = StreamState::new();
    crate::stream::run_stream_loop(
        stdout,
        log_path,
        &mut state,
        |out, line, st| process_line(out, line, st),
        |st| if st.turn_num > 0 { Some(format_summary_strip(st)) } else { None },
        |st| st.iteration_cost,
    )
}
