//! Summary input from OMP RPC. Terminal rendering is independent.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::Value;
use tiktoken_rs::o200k_base_singleton;

const ARG_TOKENS: usize = 128;
const READ_TOKENS: usize = 96;
const RESULT_TOKENS: usize = 192;
const OMITTED: &str = "\n[… omitted …]\n";

pub(super) fn token_count(text: &str) -> usize {
    o200k_base_singleton().count_ordinary(text)
}

fn excerpt(text: &str, limit: usize) -> Cow<'_, str> {
    let text = text.trim();
    if text.len() <= limit {
        return text.into();
    }
    let bpe = o200k_base_singleton();
    let tokens = bpe.encode_ordinary(text);
    if tokens.len() <= limit {
        return text.into();
    }
    let room = limit - token_count(OMITTED);
    let mut head = room / 3;
    let mut tail = room - head;
    loop {
        // Token boundaries can cut through a Unicode character.
        let first = bpe.decode(&tokens[..head]);
        let last = bpe.decode(&tokens[tokens.len() - tail..]);
        match (first, last) {
            (Ok(first), Ok(last)) => {
                let clipped = format!("{first}{OMITTED}{last}");
                if token_count(&clipped) <= limit {
                    return clipped.into();
                }
                tail -= 1;
            }
            (Err(_), _) => head -= 1,
            (_, Err(_)) => tail -= 1,
        }
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn result_text(result: &Value) -> Cow<'_, str> {
    let mut text = items(&result["content"])
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str());
    let first = text.next().unwrap_or_default();
    let Some(second) = text.next() else {
        return first.into();
    };
    let mut joined = format!("{first}\n{second}");
    for part in text {
        joined.push('\n');
        joined.push_str(part);
    }
    joined.into()
}

fn append(out: &mut String, text: &str) {
    if !text.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
}

struct Call {
    label: String,
    context: String,
}

impl Call {
    fn new(name: &str, args: &Value) -> Self {
        let field = |key| excerpt(string(args, key), ARG_TOKENS);
        let mut label = String::new();
        let mut context = String::new();
        match name {
            "read" | "write" => {
                let path = if args["path"].is_string() {
                    "path"
                } else {
                    "filePath"
                };
                label.push_str(&field(path));
                if name == "write" {
                    context.push_str(&field("content"));
                }
            }
            "grep" => {
                let pattern = field("pattern");
                let scope = field("path");
                let _ = write!(label, "{pattern:?}");
                if !scope.is_empty() {
                    let _ = write!(label, " in {scope}");
                }
            }
            "bash" => label.push_str(&field("command")),
            "edit" | "apply_patch" => {
                let patch = ["input", "patch", "diff"]
                    .into_iter()
                    .find_map(|key| args[key].as_str())
                    .unwrap_or_default();
                let targets = patch.lines().filter_map(|line| {
                    line.strip_prefix("*** Update File: ")
                        .or_else(|| line.strip_prefix("*** Add File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                        .or_else(|| line.strip_prefix("*** Move to: "))
                        .or_else(|| {
                            line.strip_prefix('[')?
                                .strip_suffix(']')?
                                .rsplit_once('#')
                                .map(|(path, _)| path)
                        })
                });
                for target in targets {
                    if !label.is_empty() {
                        label.push_str(", ");
                    }
                    label.push_str(target);
                }
                if label.is_empty() {
                    label.push_str(&field("path"));
                } else if let Cow::Owned(clipped) = excerpt(&label, ARG_TOKENS) {
                    label = clipped;
                }
                context.push_str(&excerpt(patch, ARG_TOKENS));
            }
            "task" => {
                for task in items(&args["tasks"]) {
                    let name = string(task, "name");
                    let assignment = excerpt(string(task, "task"), ARG_TOKENS);
                    if !assignment.is_empty() {
                        append(&mut context, &format!("{name}: {assignment}"));
                    }
                }
                if let Cow::Owned(clipped) = excerpt(&context, ARG_TOKENS) {
                    context = clipped;
                }
            }
            "wait" => {}
            "todo" => {
                label.push_str(&field("op"));
                if let Some(phase) = args["phase"].as_str() {
                    let _ = write!(label, " {}", excerpt(phase, ARG_TOKENS));
                }
                if let Some(task) = args["task"].as_str() {
                    context.push_str(&excerpt(task, ARG_TOKENS));
                } else if args["task"].is_array() {
                    context.push_str(&excerpt(&args["task"].to_string(), ARG_TOKENS));
                }
            }
            _ if args.as_object().is_some_and(|args| !args.is_empty()) => {
                label.push_str(&excerpt(&args.to_string(), ARG_TOKENS));
            }
            _ => {}
        }
        Self { label, context }
    }

    fn render(&self, name: &str, status: &str) -> String {
        let mut entry = String::new();
        entry.push_str(name);
        if !self.label.is_empty() {
            entry.push(' ');
            entry.push_str(&self.label);
        }
        let _ = write!(entry, " — {status}");
        append(&mut entry, &self.context);
        entry
    }
}

/// Tracks safe pause points even when transcript capture is disabled.
#[derive(Default)]
pub(super) struct WorkerStream {
    capturing: bool,
    in_message_block: bool,
    calls: HashMap<String, Option<Call>>,
    text: String,
    jobs: HashMap<String, String>,
    finished_jobs: HashSet<String>,
}

impl WorkerStream {
    pub(super) fn enable_capture(&mut self) {
        self.capturing = true;
    }

    pub(super) fn at_boundary(&self) -> bool {
        !self.in_message_block && self.calls.is_empty()
    }

    pub(super) fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    pub(super) fn reset(&mut self) {
        self.in_message_block = false;
        self.calls.clear();
        self.jobs.clear();
        self.finished_jobs.clear();
    }

    pub(super) fn observe(&mut self, frame: &Value) -> Option<String> {
        match string(frame, "type") {
            "message_update" => {
                let event = &frame["assistantMessageEvent"];
                match string(event, "type") {
                    "text_start" | "thinking_start" => self.in_message_block = true,
                    "thinking_end" => self.in_message_block = false,
                    "text_delta" => {
                        self.in_message_block = true;
                        if self.capturing {
                            self.text.push_str(string(event, "delta"));
                        }
                    }
                    "text_end" => {
                        self.in_message_block = false;
                        if self.capturing {
                            if let Some(full) = event["content"].as_str() {
                                self.text.clear();
                                self.text.push_str(full);
                            }
                            return Some(self.take_text());
                        }
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                if let Some(id) = frame["toolCallId"].as_str() {
                    let call = self
                        .capturing
                        .then(|| Call::new(string(frame, "toolName"), &frame["args"]));
                    self.calls.insert(id.to_owned(), call);
                }
            }
            "tool_execution_end" => {
                let call = self.calls.remove(string(frame, "toolCallId")).flatten();
                if self.capturing {
                    let name = string(frame, "toolName");
                    let call = call.unwrap_or_else(|| Call::new(name, &frame["args"]));
                    return Some(self.tool_result(name, &call, frame));
                }
            }
            "message_end" if self.capturing && frame["message"]["customType"] == "async-result" => {
                return self.async_result(&frame["message"]);
            }
            "message_end" if frame["message"]["role"] == "assistant" => {
                self.in_message_block = false;
                if !self.text.is_empty() {
                    return Some(self.take_text());
                }
            }
            "prompt_result" => {
                self.in_message_block = false;
                self.calls.clear();
                if !self.text.is_empty() {
                    return Some(self.take_text());
                }
            }
            _ => {}
        }
        None
    }

    fn tool_result(&mut self, name: &str, call: &Call, frame: &Value) -> String {
        let result = &frame["result"];
        let details = &result["details"];
        let error = frame["isError"] == true || result["isError"] == true;
        if !error {
            if name == "task"
                && let Some(entry) = self.task_launch(call, details)
            {
                return entry;
            }
            if details["async"]["state"] == "running"
                && let Some(id) = details["async"]["jobId"].as_str()
            {
                self.finished_jobs.remove(id);
                self.jobs.insert(id.to_owned(), call.label.clone());
                return call.render(name, &format!("started {id}; result pending"));
            }
            if name == "wait"
                && let Some(jobs) = details["jobs"].as_array()
            {
                return self.wait_result(jobs);
            }
            if name == "todo" && details["phases"].is_array() {
                return todo_result(call, details);
            }
        }
        let exit_code = details["exitCode"].as_i64().or_else(|| {
            if name == "bash" {
                details["trace"]["status"].as_i64()
            } else {
                None
            }
        });
        let status = if let Some(code) = exit_code {
            Cow::Owned(format!("exit {code}"))
        } else if error {
            Cow::Borrowed("error")
        } else {
            Cow::Borrowed("OK")
        };
        let mut entry = call.render(name, &status);
        let limit = if matches!(name, "read" | "grep") {
            READ_TOKENS
        } else {
            RESULT_TOKENS
        };
        append(&mut entry, &excerpt(&result_text(result), limit));
        entry
    }

    fn task_launch(&mut self, call: &Call, details: &Value) -> Option<String> {
        let ids: Vec<_> = items(&details["progress"])
            .iter()
            .filter_map(|job| job["id"].as_str())
            .collect();
        if ids.is_empty() {
            return None;
        }
        for &id in &ids {
            self.finished_jobs.remove(id);
        }
        Some(call.render(
            "task",
            &format!("started {}; results pending", ids.join(", ")),
        ))
    }

    fn wait_result(&mut self, jobs: &[Value]) -> String {
        let mut running = Vec::new();
        let mut finished = String::new();
        for job in jobs {
            let id = string(job, "id");
            let status = string(job, "status");
            if matches!(status, "running" | "queued" | "pending") {
                running.push(id);
            } else if !status.is_empty() && !id.is_empty() {
                let output = ["errorText", "resultText", "output", "result", "summary"]
                    .iter()
                    .find_map(|key| job[*key].as_str())
                    .unwrap_or_default();
                let entry = self.complete_job(id, job, output);
                append(&mut finished, &entry);
            }
        }
        if finished.is_empty() {
            if running.is_empty() {
                "wait — no new completion".to_owned()
            } else {
                format!(
                    "wait — no new completion; still running: {}",
                    excerpt(&running.join(", "), ARG_TOKENS)
                )
            }
        } else {
            finished
        }
    }

    fn async_result(&mut self, message: &Value) -> Option<String> {
        let jobs = items(&message["details"]["jobs"]);
        let content = string(message, "content");
        let mut entry = String::new();
        for job in jobs {
            let id = string(job, "jobId");
            if !id.is_empty() {
                append(
                    &mut entry,
                    &self.complete_job(id, job, async_output(content, id, jobs.len())),
                );
            }
        }
        (!entry.is_empty()).then_some(entry)
    }

    fn complete_job(&mut self, id: &str, job: &Value, output: &str) -> String {
        if !self.finished_jobs.insert(id.to_owned()) {
            return String::new();
        }
        let label = self.jobs.remove(id);
        let label = label
            .as_deref()
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| string(job, "label"));
        let status = string(job, "status");
        let kind = string(job, "type");
        let code = job["exitCode"].as_i64().or_else(|| {
            if kind == "bash" && status == "failed" {
                output
                    .lines()
                    .next_back()?
                    .strip_prefix("Command exited with code ")?
                    .parse()
                    .ok()
            } else {
                None
            }
        });
        let mut entry = format!("{kind} job {id} finished — {status}");
        if let Some(code) = code {
            let _ = write!(entry, "; exit {code}");
        }
        if !label.is_empty() && label != id {
            let _ = write!(entry, " ({})", excerpt(label, ARG_TOKENS));
        }
        append(&mut entry, &excerpt(output, RESULT_TOKENS));
        entry
    }
}

fn async_output<'a>(content: &'a str, id: &str, count: usize) -> &'a str {
    let output = content
        .trim_end()
        .trim_end_matches("&lt;/system-notice>")
        .trim_end_matches("</system-notice>")
        .trim_end();
    if count == 1 {
        output
            .split_once("Resume your work using the result below.\n")
            .map_or(output, |(_, body)| body)
    } else {
        output
            .split("\n── Job ")
            .skip(1)
            .find_map(|section| {
                let (header, body) = section.split_once('\n')?;
                let rest = header.strip_prefix(id)?;
                (rest == " ──" || rest.starts_with(" (")).then_some(body.trim())
            })
            .unwrap_or_default()
    }
}

fn todo_result(call: &Call, details: &Value) -> String {
    let op = string(details, "op");
    let label = if call.label.is_empty() {
        op
    } else {
        &call.label
    };
    let mut entry = format!("todo {label} — OK");
    let mut states = String::new();
    let selected = serde_json::from_str::<Vec<&str>>(&call.context).unwrap_or_default();
    let completed = items(&details["completedTasks"]);
    for phase in items(&details["phases"]) {
        let name = string(phase, "name");
        let tasks = items(&phase["tasks"]);
        if matches!(op, "init" | "view" | "append") {
            append(&mut states, &format!("{name}: {} items", tasks.len()));
            continue;
        }
        for task in tasks {
            let content = string(task, "content");
            let status = string(task, "status");
            if selected.contains(&content)
                || (!call.context.is_empty() && excerpt(content, ARG_TOKENS) == call.context)
                || (call.context.is_empty() && op == "start" && status == "in_progress")
                || completed
                    .iter()
                    .any(|done| done["phase"] == name && done["content"] == content)
            {
                append(&mut states, &format!("{name}: {content} — {status}"));
            }
        }
    }
    if states.is_empty() && !call.context.is_empty() {
        let status = match op {
            "rm" => " — removed",
            "drop" => " — dropped",
            _ => "",
        };
        append(&mut entry, &format!("{}{status}", call.context));
    } else {
        append(&mut entry, &excerpt(&states, RESULT_TOKENS));
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parallel_calls_keep_their_results_and_pause_waits_for_both() {
        let mut stream = WorkerStream::default();
        stream.enable_capture();
        for (id, path) in [("one", "one.rs"), ("two", "two.rs")] {
            stream.observe(&json!({
                "type": "tool_execution_start", "toolName": "read", "toolCallId": id,
                "args": { "path": path }
            }));
        }
        let end = |id, body| {
            json!({
                "type": "tool_execution_end", "toolName": "read", "toolCallId": id,
                "result": { "content": [{ "type": "text", "text": body }] }
            })
        };
        assert_eq!(
            stream.observe(&end("two", "second output")).unwrap(),
            "read two.rs — OK\nsecond output"
        );
        assert!(!stream.at_boundary(), "the first call is still executing");
        assert_eq!(
            stream.observe(&end("one", "first output")).unwrap(),
            "read one.rs — OK\nfirst output"
        );
        assert!(stream.at_boundary());
    }

    #[test]
    fn read_excerpt_keeps_context_and_tail_without_exceeding_its_token_limit() {
        let text = format!("First line\n{}\nlast result", "🧑🏽‍💻 details ".repeat(1000));
        let clipped = excerpt(&text, READ_TOKENS);
        assert!(clipped.starts_with("First line"));
        assert!(clipped.contains(OMITTED));
        assert!(clipped.ends_with("last result"));
        assert!(token_count(&clipped) <= READ_TOKENS);
    }

    #[test]
    fn grouped_async_results_keep_per_job_exit_codes_and_output() {
        let mut stream = WorkerStream::default();
        stream.enable_capture();
        let text = stream
            .observe(&json!({
                "type": "message_end",
                "message": {
                    "customType": "async-result",
                    "content": "2 jobs have completed.\n── Job bg_1 (first) ──\none\nCommand exited with code 17\n── Job bg_10 (second) ──\ntwo\nCommand exited with code 18\n&lt;/system-notice>",
                    "details": { "jobs": [
                        { "jobId": "bg_1", "type": "bash", "status": "failed", "label": "first" },
                        { "jobId": "bg_10", "type": "bash", "status": "failed", "label": "second" }
                    ]}
                }
            }))
            .unwrap();
        assert!(text.contains("bg_1 finished — failed; exit 17 (first)\none"));
        assert!(text.contains("bg_10 finished — failed; exit 18 (second)\ntwo"));
    }
}
