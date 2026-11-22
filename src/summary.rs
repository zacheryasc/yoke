//! Block-driven summaries for an observing OMP session.
//!
//! The main worker's NDJSON stream is split into content blocks: one per
//! text block, thinking block, or executed tool call. Every `summary-every`
//! blocks, the human-readable transcript goes to a separate cheap OMP
//! invocation. Its answer is written as one NDJSON record to the summary
//! side stream (an inherited file descriptor), never to stdout, stderr,
//! or the run artifacts.
//!
//! A single queue drains batches in order. `pending()` includes the batch
//! currently being summarized, so backpressure pauses the worker until the
//! queue is fully drained.

use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::config::{Config, Thinking};
use crate::stream_omp;
use crate::{log, log_error};

struct SummarySettings {
    model: String,
    thinking: Thinking,
}

/// A batch of rendered blocks, one string per block.
type Batch = Vec<String>;

#[derive(Default)]
struct QueueState {
    outstanding: AtomicUsize,
    stopped: AtomicBool,
}

#[derive(Default)]
struct BlockTracker {
    current: String,
    completed: Vec<String>,
    in_block: bool,
}

impl BlockTracker {
    #[cfg(test)]
    fn observe(&mut self, line: &str) -> bool {
        let frame = serde_json::from_str(line).unwrap_or(Value::Null);
        self.observe_with(Some(line), block_transition(line, &frame))
    }

    fn observe_with(&mut self, display: Option<&str>, transition: Option<Boundary>) -> bool {
        if let Some(display) = display.filter(|display| !display.is_empty()) {
            if !self.current.is_empty() {
                self.current.push('\n');
            }
            self.current.push_str(display);
        }
        let Some(transition) = transition else {
            return false;
        };
        self.in_block = transition == Boundary::Start;
        if !self.in_block && display.is_some() {
            self.completed.push(std::mem::take(&mut self.current));
        }
        !self.in_block
    }

    fn take_batch(&mut self, every: u32) -> Option<Batch> {
        (every != 0 && self.completed.len() >= every as usize)
            .then(|| std::mem::take(&mut self.completed))
    }

    /// Flush a partially streamed block plus any completed blocks that did
    /// not fill a full batch. Used when the run ends so no summary work is
    /// dropped.
    fn take_partial(&mut self) -> Option<Batch> {
        if !self.current.is_empty() {
            self.completed.push(std::mem::take(&mut self.current));
        }
        (!self.completed.is_empty()).then(|| std::mem::take(&mut self.completed))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Start,
    End,
}

/// Identify the block lifecycle event on this line.
/// A streamed text/thinking block or an executing tool call is open from
/// start to end; pause waits for the end so no block is split. `toolcall_end`
/// is not a boundary because the tool call's proposal and execution belong to
/// one block.
///
/// The substring gate preserves the boundaries recognized in raw JSONL.
fn block_transition(line: &str, frame: &Value) -> Option<Boundary> {
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
        return None;
    }
    let event = frame.get("type")?.as_str()?;
    match event {
        "tool_execution_start" => Some(Boundary::Start),
        "tool_execution_end" => Some(Boundary::End),
        "message_update" => match frame.get("assistantMessageEvent")?.get("type")?.as_str()? {
            "text_start" | "thinking_start" => Some(Boundary::Start),
            "text_end" | "thinking_end" => Some(Boundary::End),
            _ => None,
        },
        _ => None,
    }
}

pub struct SummaryEngine {
    every: Option<u32>,
    tracker: BlockTracker,
    human: stream_omp::HumanStream,
    queue: Arc<QueueState>,
    sender: Option<mpsc::Sender<Batch>>,
    writer: Option<Arc<Mutex<File>>>,
    worker: Option<JoinHandle<()>>,
    run_id: String,
    artifacts: PathBuf,
    finished: bool,
}

impl SummaryEngine {
    /// Summaries require a side-stream file descriptor and non-zero cadence.
    pub fn start(config: &Config, run_id: &str, artifacts: &Path, fd: Option<i32>) -> Self {
        let mut engine = Self {
            every: None,
            tracker: BlockTracker::default(),
            human: stream_omp::HumanStream::new(),
            queue: Arc::default(),
            sender: None,
            writer: None,
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

        let writer = Arc::new(Mutex::new(file));
        write_record(
            &writer,
            json!({
                "type": "run_start",
                "run_id": run_id,
                "artifacts": engine.artifacts,
                "summary_every": config.summary_every,
                "cwd": std::env::current_dir().ok(),
            }),
        );
        engine.writer = Some(Arc::clone(&writer));

        if config.summary_every == 0 {
            return engine;
        }
        let queue = Arc::clone(&engine.queue);
        let settings = SummarySettings {
            model: config
                .summary_model
                .as_deref()
                .unwrap_or("deepseek/deepseek-flash")
                .to_owned(),
            thinking: config.summary_thinking,
        };
        let (sender, receiver) = mpsc::channel();
        engine.sender = Some(sender);
        engine.worker = Some(thread::spawn(move || {
            summary_worker(settings, queue, writer, receiver)
        }));
        engine.every = Some(config.summary_every);
        engine
    }

    pub fn enabled(&self) -> bool {
        self.every.is_some()
    }

    /// Number of batches queued or currently being summarized.
    pub fn pending(&self) -> usize {
        self.queue.outstanding.load(Ordering::SeqCst)
    }

    /// True when no text/thinking/tool block is currently streaming, so a
    /// pause can land without splitting a block.
    pub fn at_boundary(&self) -> bool {
        !self.tracker.in_block
    }

    /// Observe a worker frame without reparsing JSON already read by RPC.
    pub fn observe(&mut self, line: &str, frame: &Value) {
        let transition = block_transition(line, frame);
        let display = self.every.map(|_| self.human.line(line));
        let ended = self.tracker.observe_with(display.as_deref(), transition);
        if ended
            && let Some(every) = self.every
            && let Some(batch) = self.tracker.take_batch(every)
        {
            log(&format!(
                "summary queued: {} blocks ({} outstanding)",
                batch.len(),
                self.pending() + 1
            ));
            self.enqueue(batch);
        }
    }

    fn enqueue(&self, batch: Batch) {
        self.queue.outstanding.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .sender
            .as_ref()
            .expect("summary worker started")
            .send(batch);
    }

    fn stop_queue(&mut self) {
        self.queue.stopped.store(true, Ordering::SeqCst);
        self.sender.take();
    }

    /// Wait for outstanding summaries, then write the terminal record and
    /// join the summarizer thread.
    pub fn finish(&mut self, outcome: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(writer) = &self.writer else {
            return;
        };
        if self.enabled() {
            if let Some(batch) = self.tracker.take_partial() {
                self.enqueue(batch);
            }
            let started = Instant::now();
            while self.pending() > 0 {
                thread::sleep(Duration::from_millis(20));
                if started.elapsed() > Duration::from_secs(600) {
                    log_error("timed out waiting for summary queue to drain");
                    break;
                }
            }
        }
        write_record(
            writer,
            json!({
                "type": "run_end",
                "run_id": self.run_id,
                "outcome": outcome,
                "artifacts": self.artifacts,
            }),
        );
        self.stop_queue();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SummaryEngine {
    fn drop(&mut self) {
        // A summarizer call can still be in flight on an aborted run. Stop
        // the queue and detach: process teardown closes the side stream; a
        // normal run joins in `finish`.
        self.stop_queue();
    }
}

fn write_record(writer: &Mutex<File>, record: Value) {
    if let Ok(mut file) = writer.lock() {
        let _ = writeln!(file, "{}", record);
        let _ = file.flush();
    }
}

fn summary_worker(
    settings: SummarySettings,
    queue: Arc<QueueState>,
    writer: Arc<Mutex<File>>,
    receiver: mpsc::Receiver<Batch>,
) {
    for batch in receiver {
        if queue.stopped.load(Ordering::SeqCst) {
            break;
        }
        let text = summarize(&settings, &batch);
        write_record(
            &writer,
            json!({
                "type": "summary",
                "blocks": batch.len(),
                "text": text,
            }),
        );
        queue.outstanding.fetch_sub(1, Ordering::SeqCst);
    }
}

fn summary_prompt(batch: &[String]) -> String {
    const MAX_RAW_BYTES: usize = 8_000;
    let mut prompt = String::from("Summarize this OMP worker transcript in under 12 lines:\n\n");
    let limit = prompt.len() + MAX_RAW_BYTES;
    for piece in batch
        .iter()
        .flat_map(|block| ["\n\n", block.as_str()])
        .skip(1)
    {
        let remaining = limit - prompt.len();
        if piece.len() <= remaining {
            prompt.push_str(piece);
        } else {
            prompt.push_str(&piece[..piece.floor_char_boundary(remaining)]);
            prompt.push_str("\n...[truncated]");
            break;
        }
    }
    prompt
}

fn summarize(settings: &SummarySettings, batch: &[String]) -> String {
    // Raw block batches can exceed ARG_MAX. Pass the prompt as an @file
    // attachment instead of one enormous argv entry.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prompt_path =
        std::env::temp_dir().join(format!("yoke-summary-{}-{}.md", std::process::id(), stamp));
    if let Err(e) = std::fs::write(&prompt_path, summary_prompt(batch)) {
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
            let stdout = String::from_utf8_lossy(&output.stdout);
            let text = stdout.trim();
            if text.is_empty() {
                format!("(summary unavailable: summarizer exited {})", output.status)
            } else {
                text.to_owned()
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

    const TEXT_END: &str = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"ok"}}"#;
    const THINKING_END: &str = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_end","contentIndex":0,"content":"hm"}}"#;
    const TOOL_END: &str = r#"{"type":"tool_execution_end","toolName":"bash","toolCallId":"t1"}"#;

    #[test]
    fn text_thinking_and_tool_calls_are_blocks() {
        let mut tracker = BlockTracker::default();
        assert!(!tracker.observe(r#"{"type":"turn_start"}"#));
        assert!(tracker.observe(TEXT_END));
        assert!(tracker.observe(THINKING_END));
        assert!(!tracker.observe(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"toolcall_end"}}"#
        ));
        assert!(tracker.observe(TOOL_END));
        assert_eq!(tracker.completed.len(), 3);
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
        tracker.observe(TEXT_END);
        assert!(!tracker.in_block, "text_end closes the block");

        tracker.observe(r#"{"type":"tool_execution_start","toolName":"read"}"#);
        assert!(tracker.in_block, "tool execution opens a block");
        tracker.observe(TOOL_END);
        assert!(!tracker.in_block, "tool_execution_end closes the block");
    }

    #[test]
    fn batches_fill_every_n_blocks() {
        let mut tracker = BlockTracker::default();
        tracker.observe(r#"{"type":"turn_start"}"#);
        tracker.observe(TEXT_END);
        tracker.observe(THINKING_END);
        assert!(tracker.take_batch(2).is_some());
        assert!(tracker.completed.is_empty());
        assert!(tracker.take_batch(2).is_none());

        tracker.observe(TOOL_END);
        let batch = tracker.take_batch(1).expect("one block batch");
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains("tool_execution_end"));
    }

    #[test]
    fn partial_batch_is_flushed() {
        let mut tracker = BlockTracker::default();
        tracker.observe(r#"{"type":"turn_start"}"#);
        tracker.observe(TEXT_END);
        tracker
            .observe(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}"#);
        let batch = tracker.take_partial().expect("partial batch");
        assert_eq!(batch.len(), 2);
        assert!(batch[1].contains("text_delta"));
        assert!(tracker.take_partial().is_none());
    }

    #[test]
    fn disabled_tracking_respects_boundaries_without_buffering() {
        let mut tracker = BlockTracker::default();
        assert!(!tracker.observe_with(None, Some(Boundary::Start)));
        assert!(tracker.in_block);
        assert!(tracker.observe_with(None, Some(Boundary::End)));
        assert!(!tracker.in_block);
        assert!(tracker.take_partial().is_none());
        assert!(tracker.observe_with(Some(""), Some(Boundary::End)));
        assert_eq!(tracker.take_batch(1).unwrap(), [""]);
    }

    #[test]
    fn prompt_caps_transcript_without_splitting_utf8() {
        let first = "x".repeat(7997);
        let prompt = summary_prompt(&[first.clone(), "é".to_owned()]);
        assert_eq!(
            prompt,
            format!(
                "Summarize this OMP worker transcript in under 12 lines:\n\n{first}\n\n\n...[truncated]"
            )
        );
    }
}
