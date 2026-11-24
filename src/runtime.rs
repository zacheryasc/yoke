use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::config::Config;
use crate::summary::SummaryEngine;
use crate::{AgentRunStats, log, log_error, signal, stream, stream_omp};

pub(crate) const YOKE_SOCKET_PATH: &str = ".loop/yoke.sock";
const INJECTION_AUDIT_PATH: &str = ".loop/injections.jsonl";
const RPC_READY_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_EVENT_POLL: Duration = Duration::from_millis(50);
// Paused workers keep injections queued, so bound the wait for delivery.
const INJECT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectionKind {
    Normal,
    Shutdown,
}

impl InjectionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Shutdown => "shutdown",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Self::Normal),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Injection {
    id: String,
    kind: InjectionKind,
    message: String,
    reply: Option<mpsc::Sender<DeliveryStatus>>,
}

impl Injection {
    fn worker_message(&self) -> String {
        let (header, directive) = match self.kind {
            InjectionKind::Normal => (
                "[YOKE HUMAN STEERING]",
                "Acknowledge this steering and apply it before continuing your current trajectory.",
            ),
            InjectionKind::Shutdown => (
                "[YOKE SHUTDOWN REQUEST]",
                "Stop starting new substantial work. Preserve useful progress, update the appropriate .loop memory files with current state, decisions, and remaining work, leave the repository coherent when practical, and exit promptly.",
            ),
        };
        format!("{header}\n{}\n\n{directive}", self.message.trim())
    }
}

#[derive(Debug)]
enum DeliveryStatus {
    Delivered,
    Failed(String),
}

#[derive(Debug)]
enum ControlCommand {
    Inject(Injection),
    Pause,
    Resume,
    Stop,
}

pub(crate) struct ControlServer {
    path: PathBuf,
    rx: mpsc::Receiver<ControlCommand>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ControlServer {
    pub(crate) fn start(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create socket parent '{}': {}",
                    parent.display(),
                    e
                )
            })?;
        }

        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(format!(
                    "another active yoke run already owns {}",
                    path.display()
                ));
            }
            fs::remove_file(&path).map_err(|e| {
                format!("failed to remove stale socket '{}': {}", path.display(), e)
            })?;
        }

        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("failed to bind {}: {}", path.display(), e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed to set {} nonblocking: {}", path.display(), e))?;

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_path = path.clone();
        let handle = thread::spawn(move || {
            let mut seq = 0;
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_control_client(stream, &tx, &mut seq),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        log_error(&format!(
                            "control socket {} accept failed: {}",
                            thread_path.display(),
                            e
                        ));
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        log(&format!("control socket listening at {}", path.display()));
        Ok(Self {
            path,
            rx,
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn handle_control_client(mut stream: UnixStream, tx: &mpsc::Sender<ControlCommand>, seq: &mut u64) {
    let response = read_control_request(&stream, seq)
        .and_then(|command| dispatch_control_request(tx, command))
        .unwrap_or_else(|error| json!({"ok": false, "status": "failed", "error": error}));
    let _ = writeln!(stream, "{}", response);
    let _ = stream.flush();
}

fn dispatch_control_request(
    tx: &mpsc::Sender<ControlCommand>,
    command: ControlCommand,
) -> Result<Value, String> {
    match command {
        ControlCommand::Inject(mut injection) => {
            append_injection_audit(&injection);
            let id = injection.id.clone();
            let kind = injection.kind;
            let (reply_tx, reply_rx) = mpsc::channel();
            injection.reply = Some(reply_tx);
            log(&format!("accepted {} steering {}", kind.as_str(), id));
            tx.send(ControlCommand::Inject(injection)).map_err(|_| {
                "active run is shutting down and cannot accept steering".to_string()
            })?;
            let (ok, status, error) = match reply_rx.recv_timeout(INJECT_RESPONSE_TIMEOUT) {
                Ok(DeliveryStatus::Delivered) => (true, "delivered", None),
                Ok(DeliveryStatus::Failed(error)) => (false, "failed", Some(error)),
                Err(_) => (true, "accepted", None),
            };
            let mut response = json!({
                "ok": ok,
                "status": status,
                "id": id,
                "kind": kind.as_str(),
            });
            if let Some(error) = error {
                response["error"] = json!(error);
            }
            Ok(response)
        }
        command => {
            let action = match command {
                ControlCommand::Pause => "pause",
                ControlCommand::Resume => "resume",
                ControlCommand::Stop => "stop",
                ControlCommand::Inject(_) => unreachable!(),
            };
            log(&format!("accepted {} request", action));
            tx.send(command)
                .map_err(|_| "active run is shutting down".to_string())?;
            Ok(json!({"ok": true, "status": "accepted", "action": action}))
        }
    }
}

fn read_control_json(stream: &UnixStream, role: &str, empty: &str) -> Result<Value, String> {
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("failed to read control {role}: {e}"))?;
    let line = line.trim();
    if line.is_empty() {
        return Err(empty.to_string());
    }
    serde_json::from_str(line).map_err(|e| format!("invalid control {role} JSON: {e}"))
}

fn read_control_request(stream: &UnixStream, seq: &mut u64) -> Result<ControlCommand, String> {
    let value = read_control_json(stream, "request", "empty control request")?;
    match value.get("type").and_then(Value::as_str) {
        Some("pause") => return Ok(ControlCommand::Pause),
        Some("resume") => return Ok(ControlCommand::Resume),
        Some("stop") => return Ok(ControlCommand::Stop),
        Some("inject") => {}
        _ => {
            return Err(
                "control request type must be 'inject', 'pause', 'resume', or 'stop'".to_string(),
            );
        }
    }
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .and_then(InjectionKind::from_str)
        .ok_or_else(|| "inject kind must be 'normal' or 'shutdown'".to_string())?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| "inject message must be a string".to_string())?
        .trim();
    if message.is_empty() {
        return Err("inject message must not be empty".to_string());
    }

    *seq += 1;
    let millis = epoch_millis();
    Ok(ControlCommand::Inject(Injection {
        id: format!("inj-{millis}-{seq}"),
        kind,
        message: message.to_string(),
        reply: None,
    }))
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

fn append_injection_audit(injection: &Injection) {
    let record = json!({
        "id": injection.id,
        "kind": injection.kind.as_str(),
        "message": injection.message,
        "accepted_at_ms": epoch_millis(),
    });
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(INJECTION_AUDIT_PATH)
    {
        let _ = writeln!(file, "{}", record);
    }
}

fn failed_run(start: std::time::Instant) -> AgentRunStats {
    AgentRunStats {
        success: false,
        wall_secs: start.elapsed().as_secs_f64(),
        stream: stream::StreamSummary::default(),
    }
}

#[derive(Default)]
pub(crate) struct RunSupervisor {
    control: Option<ControlServer>,
    worker: Option<OmpRpcWorker>,
    queued: VecDeque<Injection>,
    pub(crate) shutdown_requested: bool,
    paused: bool,
    pause_explicit: bool,
    pause_backpressure: bool,
    summary: Option<SummaryEngine>,
    summary_fd: Option<i32>,
    backup_dir: Option<PathBuf>,
    protected: Vec<String>,
}

impl RunSupervisor {
    pub(crate) fn new(summary_fd: Option<i32>) -> Self {
        Self {
            summary_fd,
            ..Self::default()
        }
    }

    pub(crate) fn ensure_control_started(&mut self) -> Result<(), String> {
        if self.control.is_none() {
            self.control = Some(ControlServer::start(YOKE_SOCKET_PATH)?);
        }
        Ok(())
    }

    /// Start the summary side stream for one run. No-op when summaries are
    /// not configured or no descriptor was supplied.
    pub(crate) fn start_summary(&mut self, config: &Config, run_id: &str, artifacts: &Path) {
        let summary = SummaryEngine::start(config, run_id, artifacts, self.summary_fd);
        if self.summary.insert(summary).enabled() {
            log(&format!(
                "summary stream enabled ({} tokens, queue limit {})",
                config.summary_tokens, config.summary_queue_limit
            ));
        }
    }

    pub(crate) fn finish_summary(&mut self, outcome: &str) {
        if let Some(summary) = self.summary.as_mut() {
            summary.finish(outcome);
        }
    }

    pub(crate) fn set_snapshot_target(&mut self, backup_dir: &Path, protected: &[&str]) {
        self.backup_dir = Some(backup_dir.to_path_buf());
        self.protected = protected.iter().map(|s| s.to_string()).collect();
    }

    pub(crate) fn reset_worker_session(&mut self) {
        self.worker = None;
        if let Some(summary) = self.summary.as_mut() {
            summary.reset_worker_session();
        }
        self.shutdown_requested = false;
        self.paused = false;
        self.pause_explicit = false;
        self.pause_backpressure = false;
    }

    pub(crate) fn invoke_worker(
        &mut self,
        config: &Config,
        prompt: &str,
        iteration: u32,
        prior_total: f64,
    ) -> AgentRunStats {
        let wall_start = std::time::Instant::now();
        let in_container = config.image.is_some();
        log(&format!(
            "Launching OMP RPC {}(iteration {})...",
            if in_container { "in container " } else { "" },
            iteration,
        ));

        if let Err(e) = self.ensure_worker(config) {
            log_error(&e);
            return failed_run(wall_start);
        }

        let log_path = config
            .log_dir
            .as_ref()
            .map(|dir| PathBuf::from(dir).join(format!("iteration-{iteration}.jsonl")));

        let worker = self.worker.as_mut().expect("worker just ensured");
        signal::set_child_pid(-(worker.child.id() as i32));

        let prompt_id = worker.next_id("prompt");
        if let Err(e) = worker.send_message_command("prompt", &prompt_id, prompt) {
            log_error(&format!("failed to send OMP RPC prompt: {}", e));
            signal::set_child_pid(0);
            return failed_run(wall_start);
        }

        let mut formatter = stream_omp::OmpStreamProcessor::new(log_path.as_deref(), prior_total);
        let mut pending: HashMap<String, PendingRpc> = HashMap::new();
        pending.insert(prompt_id.clone(), PendingRpc::Prompt);
        let mut completion = TurnCompletion::default();

        let success = loop {
            if signal::interrupted() {
                break false;
            }
            self.drain_control();

            match self.maintain_pause() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    log_error(&e);
                    break false;
                }
            }
            self.update_backpressure(config.summary_queue_limit as usize);

            self.deliver_queued(&mut pending);

            let worker = self.worker.as_mut().expect("worker");
            match worker.recv_line(RPC_EVENT_POLL) {
                Ok(Some(line)) => {
                    let frame = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if rpc_rejected(&frame, &mut pending) {
                        break false;
                    }
                    formatter.process_line(&line);
                    if let Some(summary) = self.summary.as_mut() {
                        summary.observe(&frame);
                    }
                    completion.observe(&frame, &prompt_id);
                    if completion.yielded && completion.settled {
                        break !completion.failed;
                    }
                    let at_boundary = self.summary.as_ref().is_none_or(SummaryEngine::at_boundary);
                    if at_boundary && (self.pause_explicit || self.pause_backpressure) {
                        self.set_worker_paused(true);
                    }
                }
                Ok(None) => {
                    if let Ok(Some(status)) = worker.child.try_wait() {
                        log_error(&format!("OMP RPC process exited early: {}", status));
                        break false;
                    }
                }
                Err(e) => {
                    log_error(&format!("failed to read OMP RPC stream: {}", e));
                    break false;
                }
            }
        };

        signal::set_child_pid(0);
        let summary = formatter.finish();
        let success = !signal::interrupted() && success;

        log(&format!(
            "OMP RPC turn {} ({})",
            iteration,
            if success { "success" } else { "failure" }
        ));
        AgentRunStats {
            success,
            wall_secs: wall_start.elapsed().as_secs_f64(),
            stream: summary,
        }
    }

    /// Hold the worker while paused. Returns Ok(true) when the caller should
    /// skip to the next poll iteration, Ok(false) when the loop should
    /// proceed, and Err when the paused worker died.
    fn maintain_pause(&mut self) -> Result<bool, String> {
        if !self.paused {
            return Ok(false);
        }
        // Stop wins over pause: thaw the worker so it can accept the
        // graceful shutdown steering, without clearing backpressure.
        if !self.shutdown_requested {
            let drained = !self.pause_backpressure
                || self
                    .summary
                    .as_ref()
                    .is_none_or(|summary| summary.pending() == 0);
            if !drained || self.pause_explicit {
                if let Ok(Some(status)) = self.worker.as_mut().expect("worker").child.try_wait() {
                    return Err(format!("OMP RPC process exited while paused: {}", status));
                }
                thread::sleep(RPC_EVENT_POLL);
                return Ok(true);
            }
            self.pause_backpressure = false;
        }
        self.set_worker_paused(false);
        Ok(false)
    }

    fn update_backpressure(&mut self, limit: usize) {
        let Some(summary) = self.summary.as_ref().filter(|s| s.enabled()) else {
            return;
        };
        let pending = summary.pending();
        if pending == 0 {
            self.pause_backpressure = false;
        } else if pending >= limit {
            if !self.pause_backpressure {
                log(&format!(
                    "summary backlog reached {}; pausing at the next block boundary",
                    pending
                ));
            }
            self.pause_backpressure = true;
        }
    }

    fn ensure_worker(&mut self, config: &Config) -> Result<(), String> {
        if self.worker.is_none() {
            self.worker = Some(OmpRpcWorker::start(config)?);
        }
        Ok(())
    }

    fn set_worker_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        if !paused && let Some(backup_dir) = self.backup_dir.as_deref() {
            crate::snapshot_files_to(backup_dir, &self.protected);
        }
        let Some(worker) = self.worker.as_ref() else {
            self.paused = false;
            return;
        };
        let pid = worker.child.id();
        let pgid = -(pid as i32);
        let (changed, action, message) = if paused {
            (
                signal::stop_child(pgid),
                "pause",
                "worker paused at block boundary",
            )
        } else {
            (
                signal::continue_child(pgid),
                "resume",
                "worker resumed; protected files re-snapshotted",
            )
        };
        if changed {
            self.paused = paused;
            log(message);
        } else {
            log_error(&format!("failed to {action} worker process {pid}"));
        }
    }

    fn drain_control(&mut self) {
        let Some(control) = self.control.as_ref() else {
            return;
        };
        while let Ok(command) = control.rx.try_recv() {
            match command {
                ControlCommand::Inject(injection) => {
                    log(&format!(
                        "queued {} steering {}",
                        injection.kind.as_str(),
                        injection.id
                    ));
                    self.queued.push_back(injection);
                }
                ControlCommand::Pause => {
                    self.pause_explicit = true;
                    log("pause requested; will pause at the next block boundary");
                }
                ControlCommand::Resume => {
                    self.pause_explicit = false;
                    log("resume requested");
                }
                ControlCommand::Stop => {
                    self.shutdown_requested = true;
                    self.pause_explicit = false;
                    self.queued.push_back(Injection {
                        id: format!("stop-{}", epoch_millis()),
                        kind: InjectionKind::Shutdown,
                        message: "Stop the run gracefully and report the final result.".to_string(),
                        reply: None,
                    });
                    log("stop requested; delivering graceful shutdown");
                }
            }
        }
    }

    fn deliver_queued(&mut self, pending: &mut HashMap<String, PendingRpc>) {
        let worker = self.worker.as_mut().expect("worker");
        while let Some(injection) = self.queued.pop_front() {
            let rpc_id = worker.next_id("steer");
            self.shutdown_requested |= injection.kind == InjectionKind::Shutdown;
            match worker.send_message_command("steer", &rpc_id, &injection.worker_message()) {
                Ok(()) => {
                    log(&format!(
                        "delivering {} steering {} to OMP RPC",
                        injection.kind.as_str(),
                        injection.id
                    ));
                    pending.insert(
                        rpc_id,
                        PendingRpc::Injection {
                            id: injection.id,
                            kind: injection.kind,
                            reply: injection.reply,
                        },
                    );
                }
                Err(e) => {
                    log_error(&format!(
                        "failed to deliver {} steering {}: {}",
                        injection.kind.as_str(),
                        injection.id,
                        e
                    ));
                    if let Some(reply) = injection.reply {
                        let _ = reply.send(DeliveryStatus::Failed(e.to_string()));
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct TurnCompletion {
    yielded: bool,
    settled: bool,
    failed: bool,
}

impl TurnCompletion {
    /// A prompt is finished at its `prompt_result`, but the session is only safe
    /// for the next prompt after `session_settled`. Non-terminal `agent_end`
    /// frames (retry, compaction, TTSR continuation) must not complete a turn.
    fn observe(&mut self, frame: &Value, prompt_id: &str) {
        match frame.get("type").and_then(Value::as_str) {
            Some("prompt_result") if frame.get("id").and_then(Value::as_str) == Some(prompt_id) => {
                self.yielded = true;
                self.settled = frame.get("sessionSettled").and_then(Value::as_bool) == Some(true);
                self.failed |= frame.get("status").and_then(Value::as_str) != Some("completed");
            }
            Some("session_settled") => self.settled = true,
            _ => {}
        }
    }
}

#[derive(Debug)]
enum PendingRpc {
    Prompt,
    Injection {
        id: String,
        kind: InjectionKind,
        reply: Option<mpsc::Sender<DeliveryStatus>>,
    },
}

fn rpc_rejected(frame: &Value, pending: &mut HashMap<String, PendingRpc>) -> bool {
    if frame.get("type").and_then(Value::as_str) != Some("response") {
        return false;
    }
    let Some(request) = frame
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| pending.remove(id))
    else {
        return false;
    };
    let error = (frame.get("success").and_then(Value::as_bool) != Some(true)).then(|| {
        frame
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown RPC error")
    });
    match request {
        PendingRpc::Prompt => {
            if let Some(error) = error {
                log_error(&format!("OMP RPC rejected prompt: {}", error));
            }
        }
        PendingRpc::Injection { id, kind, reply } => {
            let kind = kind.as_str();
            if let Some(error) = error {
                log_error(&format!("OMP RPC rejected {kind} steering {id}: {error}"));
            } else {
                log(&format!("delivered {kind} steering {id} to OMP RPC"));
            }
            if let Some(reply) = reply {
                let _ = reply.send(error.map_or_else(
                    || DeliveryStatus::Delivered,
                    |error| DeliveryStatus::Failed(error.to_string()),
                ));
            }
        }
    }
    error.is_some()
}

struct OmpRpcWorker {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl OmpRpcWorker {
    fn start(config: &Config) -> Result<Self, String> {
        let mut cmd = crate::omp_command(
            config,
            "rpc",
            config.model.as_deref(),
            config.thinking.map(|level| level.as_omp_arg()),
        );
        // Own process group: pause can freeze tool children along with the
        // RPC process, and Ctrl+C can terminate the whole tree.
        cmd.process_group(0);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!(
                    "failed to spawn '{}': {}",
                    cmd.get_program().to_string_lossy(),
                    e
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to capture OMP RPC stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture OMP RPC stdout".to_string())?;
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .filter(|line| !line.trim().is_empty())
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut worker = Self {
            child,
            stdin,
            lines: rx,
            reader: Some(reader),
            next_id: 0,
        };
        worker.wait_ready()?;
        Ok(worker)
    }

    fn wait_ready(&mut self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + RPC_READY_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("OMP RPC exited before ready: {}", status));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for OMP RPC readiness".to_string());
            }
            let line = self
                .recv_line(remaining.min(RPC_EVENT_POLL))
                .map_err(|error| format!("{error} before ready"))?;
            if line.is_some_and(|line| {
                serde_json::from_str::<Value>(&line)
                    .is_ok_and(|frame| frame.get("type").and_then(Value::as_str) == Some("ready"))
            }) {
                return Ok(());
            }
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("yoke-{}-{}", prefix, self.next_id)
    }

    fn send_message_command(&mut self, command: &str, id: &str, message: &str) -> io::Result<()> {
        let request = json!({
            "type": command,
            "id": id,
            "message": message,
        });
        writeln!(self.stdin, "{}", request)?;
        self.stdin.flush()
    }

    fn recv_line(&self, timeout: Duration) -> Result<Option<String>, &'static str> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Ok(Some(line)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("OMP RPC stream closed"),
        }
    }
}

impl Drop for OmpRpcWorker {
    fn drop(&mut self) {
        signal::terminate_group(-(self.child.id() as i32));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub(crate) fn send_inject_command(kind: InjectionKind, message: &str) -> Result<Value, String> {
    send_control_command(json!({
        "type": "inject",
        "kind": kind.as_str(),
        "message": message,
    }))
}

/// Send a pause/resume/stop control request to the active run.
pub(crate) fn send_command(action: &str) -> Result<Value, String> {
    send_control_command(json!({ "type": action }))
}

fn send_control_command(request: Value) -> Result<Value, String> {
    let path = Path::new(YOKE_SOCKET_PATH);
    if !path.exists() {
        return Err(format!(
            "no active yoke run found for this project ({} does not exist)",
            path.display()
        ));
    }
    let mut stream = UnixStream::connect(path).map_err(|e| {
        format!(
            "failed to connect to active yoke run at {}: {}",
            path.display(),
            e
        )
    })?;
    writeln!(stream, "{}", request)
        .and_then(|_| stream.flush())
        .map_err(|e| format!("failed to send control request: {}", e))?;

    read_control_json(
        &stream,
        "response",
        "active yoke run closed the control response",
    )
}
