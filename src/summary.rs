//! Block-driven summaries for an observing OMP session.
//!
//! The main worker's NDJSON stream is split into content blocks: one per
//! text block, thinking block, or executed tool call. Every `summary-every`
//! blocks, the raw JSONL for those blocks is handed to a separate cheap OMP
//! invocation. Its freeform answer is written as one NDJSON record to the
//! summary side stream (an inherited file descriptor), never to stdout,
//! stderr, or the run artifacts.
//!
//! A single queue drains batches in order. `pending()` includes the batch
//! currently being summarized, so backpressure pauses the worker until the
//! queue is fully drained.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::config::{Config, Thinking};
use crate::stream_omp;
use crate::{log, log_error};

#[derive(Debug, Clone)]
struct SummarySettings {
    every: u32,
    model: String,
    thinking: Thinking,
}

/// A batch of raw blocks, already joined into one JSONL string per block.
type Batch = Vec<String>;

struct Queue {
    pending: Mutex<VecDeque<Batch>>,
    cv: Condvar,
    outstanding: AtomicUsize,
    stopped: AtomicBool,
}

impl Queue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            outstanding: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    fn push(&self, batch: Batch) {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        self.pending
            .lock()
            .expect("summary queue lock")
            .push_back(batch);
        self.cv.notify_one();
    }

    fn pop(&self) -> Option<Batch> {
        let mut pending = self.pending.lock().expect("summary queue lock");
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(batch) = pending.pop_front() {
                return Some(batch);
            }
            pending = self.cv.wait(pending).expect("summary queue wait");
        }
    }

    fn complete(&self) {
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
    }

    fn pending(&self) -> usize {
        self.outstanding.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }
}

#[derive(Default)]
struct BlockTracker {
    current: String,
    completed: Vec<String>,
    blocks_since_batch: u32,
    in_block: bool,
}

impl BlockTracker {
    #[cfg(test)]
    fn observe(&mut self, line: &str) -> bool {
        let (started, ended) = block_transition(line);
        self.observe_with(line, started, ended)
    }

    fn observe_with(&mut self, display: &str, started: bool, ended: bool) -> bool {
        if !display.is_empty() {
            if !self.current.is_empty() {
                self.current.push('\n');
            }
            self.current.push_str(display);
        }
        if started {
            self.in_block = true;
        }
        if ended {
            self.in_block = false;
            self.completed.push(std::mem::take(&mut self.current));
            self.blocks_since_batch += 1;
        }
        ended
    }

    fn take_batch(&mut self, every: u32) -> Option<Batch> {
        if every == 0 || self.blocks_since_batch < every {
            return None;
        }
        self.blocks_since_batch = 0;
        Some(std::mem::take(&mut self.completed))
    }

    /// Flush a partially streamed block plus any completed blocks that did
    /// not fill a full batch. Used when the run ends so no summary work is
    /// dropped.
    fn take_partial(&mut self) -> Option<Batch> {
        if !self.current.is_empty() {
            self.completed.push(std::mem::take(&mut self.current));
        }
        self.blocks_since_batch = 0;
        if self.completed.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.completed))
        }
    }
}

fn event_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

fn assistant_event_type(value: &Value) -> Option<&str> {
    value
        .get("assistantMessageEvent")
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
}

/// Return `(started, ended)` for the block lifecycle event on this line.
/// A streamed text/thinking block or an executing tool call is open from
/// start to end; pause waits for the end so no block is split. `toolcall_end`
/// is not a boundary because the tool call's proposal and execution belong to
/// one block.
///
/// Most stream lines are deltas. The substring gate avoids parsing those as
/// JSON on the worker's hot path.
fn block_transition(line: &str) -> (bool, bool) {
    let relevant = [
        "text_start",
        "text_end",
        "thinking_start",
        "thinking_end",
        "tool_execution_start",
        "tool_execution_end",
    ]
    .iter()
    .any(|name| line.contains(name));
    if !relevant {
        return (false, false);
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return (false, false);
    };
    let started = match event_type(&value) {
        Some("tool_execution_start") => true,
        Some("message_update") => matches!(
            assistant_event_type(&value),
            Some("text_start" | "thinking_start")
        ),
        _ => false,
    };
    let ended = match event_type(&value) {
        Some("tool_execution_end") => true,
        Some("message_update") => matches!(
            assistant_event_type(&value),
            Some("text_end" | "thinking_end")
        ),
        _ => false,
    };
    (started, ended)
}

pub struct SummaryEngine {
    settings: Option<SummarySettings>,
    tracker: BlockTracker,
    human: stream_omp::HumanStream,
    queue: Arc<Queue>,
    writer: Arc<Mutex<Option<File>>>,
    worker: Option<JoinHandle<()>>,
    run_id: String,
    artifacts: PathBuf,
    finished: bool,
}

impl SummaryEngine {
    /// Build the engine. Summaries are enabled only when a side-stream file
    /// descriptor was supplied, `summary-every` is non-zero, and a cheap
    /// `summary-model` is configured.
    pub fn start(config: &Config, run_id: &str, artifacts: &Path, fd: Option<i32>) -> Self {
        let settings = config.summary_model.as_ref().map(|model| SummarySettings {
            every: config.summary_every,
            model: model.clone(),
            thinking: config.summary_thinking,
        });

        let mut engine = Self {
            settings: None,
            tracker: BlockTracker::default(),
            human: stream_omp::HumanStream::new(),
            queue: Arc::new(Queue::new()),
            writer: Arc::new(Mutex::new(None)),
            worker: None,
            run_id: run_id.to_string(),
            artifacts: artifacts.to_path_buf(),
            finished: false,
        };

        let Some(fd) = fd else {
            return engine;
        };
        let file = unsafe { open_inherited_fd(fd) };
        let Some(file) = file else {
            log_error(&format!(
                "summary side stream fd {} is not writable; summaries disabled",
                fd
            ));
            return engine;
        };

        engine.writer = Arc::new(Mutex::new(Some(file)));
        engine.write_record(json!({
            "type": "run_start",
            "run_id": run_id,
            "artifacts": engine.artifacts,
            "summary_every": settings.as_ref().map(|s| s.every),
            "cwd": std::env::current_dir().ok(),
        }));

        let Some(settings) = settings else {
            if config.summary_every > 0 {
                log_error(
                    "summary side stream is open but summary-model is not set; summaries disabled",
                );
            }
            return engine;
        };
        if settings.every == 0 {
            return engine;
        }
        let queue = Arc::clone(&engine.queue);
        let writer = Arc::clone(&engine.writer);
        let thread_settings = settings.clone();
        engine.worker = Some(thread::spawn(move || {
            summary_worker(thread_settings, queue, writer)
        }));
        engine.settings = Some(settings);
        engine
    }

    pub fn enabled(&self) -> bool {
        self.settings.is_some()
    }

    /// Number of batches queued or currently being summarized.
    pub fn pending(&self) -> usize {
        self.queue.pending()
    }

    /// True when no text/thinking/tool block is currently streaming, so a
    /// pause can land without splitting a block.
    pub fn at_boundary(&self) -> bool {
        !self.tracker.in_block
    }

    /// Feed one raw OMP line. Returns true when the line completed a block.
    pub fn observe(&mut self, line: &str) -> bool {
        let (started, ended) = block_transition(line);
        if self.settings.is_none() {
            if started {
                self.tracker.in_block = true;
            }
            if ended {
                self.tracker.in_block = false;
            }
            return ended;
        }
        let Some(settings) = self.settings.as_ref() else {
            return ended;
        };
        let display = self.human.line(line);
        self.tracker.observe_with(&display, started, ended);
        if ended && let Some(batch) = self.tracker.take_batch(settings.every) {
            log(&format!(
                "summary queued: {} blocks ({} outstanding)",
                batch.len(),
                self.queue.pending() + 1
            ));
            self.queue.push(batch);
        }
        ended
    }

    /// Wait for outstanding summaries, then write the terminal record and
    /// join the summarizer thread.
    pub fn finish(&mut self, outcome: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.writer.lock().is_ok_and(|writer| writer.is_none()) {
            return;
        }
        if self.settings.is_some() {
            if let Some(batch) = self.tracker.take_partial() {
                self.queue.push(batch);
            }
            let started = Instant::now();
            while self.queue.pending() > 0 {
                thread::sleep(Duration::from_millis(20));
                if started.elapsed() > Duration::from_secs(600) {
                    log_error("timed out waiting for summary queue to drain");
                    break;
                }
            }
        }
        self.write_record(json!({
            "type": "run_end",
            "run_id": self.run_id,
            "outcome": outcome,
            "artifacts": self.artifacts,
        }));
        self.queue.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn write_record(&self, record: Value) {
        if let Ok(mut writer) = self.writer.lock()
            && let Some(file) = writer.as_mut()
        {
            let _ = writeln!(file, "{}", record);
            let _ = file.flush();
        }
    }
}

impl Drop for SummaryEngine {
    fn drop(&mut self) {
        // A summarizer call can still be in flight on an aborted run. Stop
        // the queue and detach: process teardown closes the side stream; a
        // normal run joins in `finish`.
        self.queue.stop();
        self.worker.take();
    }
}

fn summary_worker(settings: SummarySettings, queue: Arc<Queue>, writer: Arc<Mutex<Option<File>>>) {
    while let Some(batch) = queue.pop() {
        let text = summarize(&settings, &batch);
        if let Ok(mut writer) = writer.lock()
            && let Some(file) = writer.as_mut()
        {
            let record = json!({
                "type": "summary",
                "blocks": batch.len(),
                "text": text,
            });
            let _ = writeln!(file, "{}", record);
            let _ = file.flush();
        }
        queue.complete();
    }
}

fn summarize(settings: &SummarySettings, batch: &[String]) -> String {
    let mut raw = batch.join("\n\n");
    const MAX_RAW_BYTES: usize = 8_000;
    if raw.len() > MAX_RAW_BYTES {
        let mut cut = MAX_RAW_BYTES;
        while !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        raw.truncate(cut);
        raw.push_str("\n...[truncated]");
    }
    let prompt = format!(
        "Summarize this OMP worker transcript in under 12 lines:\n\n{}",
        raw
    );
    // Raw block batches can exceed ARG_MAX. Pass the prompt as an @file
    // attachment instead of one enormous argv entry.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prompt_path =
        std::env::temp_dir().join(format!("yoke-summary-{}-{}.md", std::process::id(), stamp));
    if let Err(e) = std::fs::write(&prompt_path, prompt) {
        return format!("(summary unavailable: failed to write prompt: {})", e);
    }
    let prompt_arg = format!("@{}", prompt_path.display());
    let output = Command::new("omp")
        .args([
            "--mode",
            "text",
            "--system-prompt",
            "You summarize OMP worker transcripts. Be terse.",
            "--no-session",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-rules",
            "--no-context-files",
            "--no-lsp",
            "--no-title",
            "--model",
            &settings.model,
            "--thinking",
            settings.thinking.as_omp_arg(),
            "-p",
            &prompt_arg,
        ])
        .env_remove("PI_SESSION_FILE")
        .env_remove("PI_SESSION_ID")
        .env_remove("PI_TOOL_CALL_ID")
        .env_remove("PI_TERMINAL_OWNER_PID")
        .env_remove("AGENT")
        .env_remove("OMPCODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output();
    let _ = std::fs::remove_file(&prompt_path);

    match output {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                format!("(summary unavailable: summarizer exited {})", output.status)
            } else {
                text
            }
        }
        Err(e) => format!("(summary unavailable: failed to run omp: {})", e),
    }
}

unsafe fn open_inherited_fd(fd: i32) -> Option<File> {
    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    if fd < 0 {
        return None;
    }
    unsafe {
        // F_GETFD both validates the descriptor and prevents wrapping a
        // closed fd, which would later close whichever fd reuses the number.
        if fcntl(fd, F_GETFD, 0) < 0 {
            return None;
        }
        if fcntl(fd, F_SETFD, FD_CLOEXEC) < 0 {
            return None;
        }
        Some(File::from_raw_fd(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_end() -> String {
        json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_end", "contentIndex": 0, "content": "ok"}
        })
        .to_string()
    }

    fn thinking_end() -> String {
        json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_end", "contentIndex": 0, "content": "hm"}
        })
        .to_string()
    }

    fn tool_end() -> String {
        json!({"type": "tool_execution_end", "toolName": "bash", "toolCallId": "t1"}).to_string()
    }

    #[test]
    fn text_thinking_and_tool_calls_are_blocks() {
        let mut tracker = BlockTracker::default();
        assert!(!tracker.observe(r#"{"type":"turn_start"}"#));
        assert!(tracker.observe(&text_end()));
        assert!(tracker.observe(&thinking_end()));
        assert!(!tracker.observe(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"toolcall_end"}}"#
        ));
        assert!(tracker.observe(&tool_end()));
        assert_eq!(tracker.blocks_since_batch, 3);
    }

    #[test]
    fn open_text_block_blocks_a_pause() {
        let mut tracker = BlockTracker::default();
        assert!(!tracker.in_block);
        tracker
            .observe(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_start"}}"#);
        assert!(tracker.in_block, "text_start opens a block");
        tracker
            .observe(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}"#);
        assert!(tracker.in_block, "deltas keep the block open");
        tracker.observe(&text_end());
        assert!(!tracker.in_block, "text_end closes the block");

        tracker.observe(r#"{"type":"tool_execution_start","toolName":"read"}"#);
        assert!(tracker.in_block, "tool execution opens a block");
        tracker.observe(&tool_end());
        assert!(!tracker.in_block, "tool_execution_end closes the block");
    }

    #[test]
    fn batches_fill_every_n_blocks() {
        let mut tracker = BlockTracker::default();
        tracker.observe(r#"{"type":"turn_start"}"#);
        tracker.observe(&text_end());
        tracker.observe(&thinking_end());
        assert!(tracker.take_batch(2).is_some());
        assert_eq!(tracker.blocks_since_batch, 0);
        assert!(tracker.take_batch(2).is_none());

        tracker.observe(&tool_end());
        let batch = tracker.take_batch(1).expect("one block batch");
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains("tool_execution_end"));
    }

    #[test]
    fn partial_batch_is_flushed() {
        let mut tracker = BlockTracker::default();
        tracker.observe(r#"{"type":"turn_start"}"#);
        tracker.observe(&text_end());
        tracker
            .observe(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}"#);
        let batch = tracker.take_partial().expect("partial batch");
        assert_eq!(batch.len(), 2);
        assert!(batch[1].contains("text_delta"));
        assert!(tracker.take_partial().is_none());
    }
}
