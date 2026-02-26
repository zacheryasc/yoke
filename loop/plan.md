Richer Stdout Display

Deliverable: Improve the NDJSON stream filter so the operator can follow Claude's
full reasoning. Currently assistant text is truncated to 3 lines / 120 chars and
many event types are silently dropped. No new crates — pure std only.

---
Context

The stream filter in src/stream.rs reads Claude's NDJSON output and formats it
as ANSI-colored terminal output. Current problems:

1. Assistant text is hard-capped at 3 lines and 120 chars per line. This cuts off
   Claude's thinking and explanations — the operator has no idea what it's doing.

2. The `stream_event` wrapper type (the majority of NDJSON lines) is silently
   dropped. These carry `content_block_delta` events with partial text and input
   JSON, which are useful for streaming progress.

3. Tool results are invisible — the `user` event type (which carries tool_result
   content) is never matched.

Goal: Show Claude's full text output, stream partial text deltas as they arrive,
and show a brief summary of tool results.

---
Implementation Stages


Stage A: Remove text truncation, show full assistant output

Objective: Stop cutting off assistant text so the operator sees everything.

src/stream.rs — in the `Some("assistant")` branch, the text display path:

  Current code truncates:
    for text_line in trimmed.lines().take(3) {
        let truncated = if text_line.len() > 120 { &text_line[..120] } else { text_line };
        println!("{}  {}{}", DIM, truncated, RESET);
    }

  Change to print all lines with no length limit:
    for text_line in trimmed.lines() {
        println!("{}  {}{}", DIM, text_line, RESET);
    }

Verify: cargo check -p yoke passes.


Stage B: Display streaming text deltas

Objective: Show `content_block_delta` events so text streams to the terminal
as Claude thinks, rather than appearing only in the final `assistant` message.

src/stream.rs — add a `Some("stream_event")` match arm. These lines wrap inner
events in a `{"type":"stream_event","event":{...}}` envelope.

The inner event types to handle:

  `content_block_delta` with `"text_delta"`:
    The delta has `{"type":"text_delta","text":"..."}`. Extract the `text` field
    from the inner delta and print it inline (no newline — use `print!` not
    `println!`) so streaming text accumulates naturally:
      print!("{}{}{}", DIM, text, RESET);
    Flush stdout after each delta.

  `content_block_delta` with `"input_json_delta"`:
    These are partial tool input being streamed. Skip these silently — the
    complete tool call will be shown when the full `assistant` event arrives.

  `content_block_start`:
    If it contains `"tool_use"`, print nothing (the full assistant event will
    show the tool call). If it contains `"text"`, print a newline to start
    a fresh line for streaming text.

  `content_block_stop`:
    Print a newline to terminate any streaming text on the current line.

  `message_start`, `message_delta`, `message_stop`:
    Skip silently — these are bookkeeping.

To detect the inner event type, use extract_str on the line for the `"type"`
field inside `"event"`. Since the line has a top-level `"type":"stream_event"`
and an inner `"type":"content_block_delta"` (etc.), and extract_str now retries
past value matches, you can search for the inner type by looking for specific
strings:
  - line.contains("\"content_block_delta\"") → delta handling
  - line.contains("\"content_block_start\"") → start handling
  - line.contains("\"content_block_stop\"") → stop handling
  - Otherwise → skip

For text_delta extraction: search for `"text_delta"` in the line, then extract
the `"text"` field. Since the line may have multiple `"text"` keys (the delta
type and the actual text content), extract_str's retry logic will handle this.

Important: flush stdout after each print! call so streaming text appears
immediately:
  use std::io::stdout;
  stdout().flush().ok();

Verify: cargo check -p yoke passes.


Stage C: Show tool result summaries

Objective: When Claude reads a file or runs a command, show a brief summary of
the tool result so the operator knows what happened.

src/stream.rs — add a `Some("user")` match arm. User events carry tool results
in the format:
  {"type":"user","message":{"role":"user","content":[{"tool_use_id":"...","type":"tool_result","content":"..."}]}}

For tool results:
  - Extract the tool_use_id (not critical but nice)
  - Check if the line contains `"tool_result"`
  - Show a brief dim summary: the first 120 chars of the content, or just
    "[tool result]" if content can't be extracted
  - Format: `println!("{}  ← result ({}b){}", DIM, content_len, RESET)`
    where content_len is the approximate length of the result content

Keep it simple — just indicate a result came back and roughly how big it was.
The exact content is less important than knowing it happened.

Verify: cargo check -p yoke passes.

---
Files to modify

| File | Change |
|------|--------|
| src/stream.rs | Remove truncation, add stream_event + user event handling |

---
Constraints

No external crates. Edition 2024. Pure std only.
Only modify src/stream.rs.

---
Success Criteria

1. cargo check -p yoke passes
2. Assistant text displays in full — no line count or character limit
3. Streaming text deltas appear as Claude thinks
4. Tool results show a brief acknowledgment line
