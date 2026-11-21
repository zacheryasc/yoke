use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
// Injections stay queued while the worker is paused, so the client must get
// an immediate `accepted` instead of waiting for delivery.
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
        match self.kind {
            InjectionKind::Normal => format!(
                "[YOKE HUMAN STEERING]\n{}\n\nAcknowledge this steering and apply it before continuing your current trajectory.",
                self.message.trim()
            ),
            InjectionKind::Shutdown => format!(
                "[YOKE SHUTDOWN REQUEST]\n{}\n\nStop starting new substantial work. Preserve useful progress, update the appropriate .loop memory files with current state, decisions, and remaining work, leave the repository coherent when practical, and exit promptly.",
                self.message.trim()
            ),
        }
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
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(format!(
                        "another active yoke run already owns {}",
                        path.display()
                    ));
                }
                Err(_) => {
                    fs::remove_file(&path).map_err(|e| {
                        format!("failed to remove stale socket '{}': {}", path.display(), e)
                    })?;
                }
            }
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
        let seq = Arc::new(AtomicU64::new(0));
        let thread_seq = Arc::clone(&seq);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_control_client(stream, &tx, &thread_seq),
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

    fn try_recv(&self) -> Option<ControlCommand> {
        match self.rx.try_recv() {
            Ok(command) => Some(command),
            Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => None,
        }
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

fn handle_control_client(
    mut stream: UnixStream,
    tx: &mpsc::Sender<ControlCommand>,
    seq: &AtomicU64,
) {
    let response = match read_control_request(&stream, seq) {
        Ok(ControlCommand::Inject(mut injection)) => {
            append_injection_audit(&injection);
            let id = injection.id.clone();
            let kind = injection.kind;
            let (reply_tx, reply_rx) = mpsc::channel();
            injection.reply = Some(reply_tx);
            log(&format!("accepted {} steering {}", kind.as_str(), id));
            match tx.send(ControlCommand::Inject(injection)) {
                Ok(()) => match reply_rx.recv_timeout(INJECT_RESPONSE_TIMEOUT) {
                    Ok(DeliveryStatus::Delivered) => json!({
                        "ok": true,
                        "status": "delivered",
                        "id": id,
                        "kind": kind.as_str(),
                    }),
                    Ok(DeliveryStatus::Failed(error)) => json!({
                        "ok": false,
                        "status": "failed",
                        "id": id,
                        "kind": kind.as_str(),
                        "error": error,
                    }),
                    Err(mpsc::RecvTimeoutError::Timeout) => json!({
                        "ok": true,
                        "status": "accepted",
                        "id": id,
                        "kind": kind.as_str(),
                    }),
                    Err(mpsc::RecvTimeoutError::Disconnected) => json!({
                        "ok": true,
                        "status": "accepted",
                        "id": id,
                        "kind": kind.as_str(),
                    }),
                },
                Err(_) => json!({
                    "ok": false,
                    "status": "failed",
                    "error": "active run is shutting down and cannot accept steering",
                }),
            }
        }
        Ok(command) => {
            let action = match command {
                ControlCommand::Pause => "pause",
                ControlCommand::Resume => "resume",
                ControlCommand::Stop => "stop",
                ControlCommand::Inject(_) => unreachable!(),
            };
            log(&format!("accepted {} request", action));
            match tx.send(command) {
                Ok(()) => json!({"ok": true, "status": "accepted", "action": action}),
                Err(_) => json!({
                    "ok": false,
                    "status": "failed",
                    "error": "active run is shutting down",
                }),
            }
        }
        Err(error) => json!({
            "ok": false,
            "status": "failed",
            "error": error,
        }),
    };

    let _ = writeln!(stream, "{}", response);
    let _ = stream.flush();
}

fn read_control_request(stream: &UnixStream, seq: &AtomicU64) -> Result<ControlCommand, String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("failed to read control request: {}", e))?,
    );
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("failed to read control request: {}", e))?;
    if line.trim().is_empty() {
        return Err("empty control request".to_string());
    }

    let value: Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid control request JSON: {}", e))?;
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
        .trim()
        .to_string();
    if message.is_empty() {
        return Err("inject message must not be empty".to_string());
    }

    let n = seq.fetch_add(1, Ordering::Relaxed) + 1;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(ControlCommand::Inject(Injection {
        id: format!("inj-{}-{}", millis, n),
        kind,
        message,
        reply: None,
    }))
}

fn append_injection_audit(injection: &Injection) {
    let record = json!({
        "id": injection.id,
        "kind": injection.kind.as_str(),
        "message": injection.message,
        "accepted_at_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    });
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(INJECTION_AUDIT_PATH)
    {
        let _ = writeln!(file, "{}", record);
    }
}

pub(crate) struct RunSupervisor {
    control: Option<ControlServer>,
    worker: Option<OmpRpcWorker>,
    queued: VecDeque<Injection>,
    shutdown_requested: bool,
    paused: bool,
    pause_explicit: bool,
    pause_backpressure: bool,
    summary: Option<SummaryEngine>,
    summary_fd: Option<i32>,
    backup_dir: Option<PathBuf>,
    protected: Vec<String>,
}

impl RunSupervisor {
    pub(crate) fn new() -> Self {
        Self {
            control: None,
            worker: None,
            queued: VecDeque::new(),
            shutdown_requested: false,
            paused: false,
            pause_explicit: false,
            pause_backpressure: false,
            summary: None,
            summary_fd: None,
            backup_dir: None,
            protected: Vec::new(),
        }
    }

    pub(crate) fn ensure_control_started(&mut self) -> Result<(), String> {
        if self.control.is_none() {
            self.control = Some(ControlServer::start(YOKE_SOCKET_PATH)?);
        }
        Ok(())
    }

    pub(crate) fn set_summary_fd(&mut self, fd: Option<i32>) {
        self.summary_fd = fd;
    }

    /// Start the summary side stream for one run. No-op when summaries are
    /// not configured or no descriptor was supplied.
    pub(crate) fn start_summary(&mut self, config: &Config, run_id: &str, artifacts: &Path) {
        self.summary = Some(SummaryEngine::start(
            config,
            run_id,
            artifacts,
            self.summary_fd,
        ));
        if self.summary.as_ref().is_some_and(SummaryEngine::enabled) {
            log(&format!(
                "summary stream enabled (every {} blocks, limit {})",
                config.summary_every, config.summary_queue_limit
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
        self.stop_worker();
        self.shutdown_requested = false;
        self.paused = false;
        self.pause_explicit = false;
        self.pause_backpressure = false;
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub(crate) fn invoke_worker(
        &mut self,
        config: &Config,
        prompt: &str,
        label: &str,
        log_prefix: &str,
        iteration: u32,
        prior_total: f64,
    ) -> AgentRunStats {
        let wall_start = std::time::Instant::now();
        let in_container = config.image.is_some();
        log(&format!(
            "Launching {} RPC {}(iteration {})...",
            label,
            if in_container { "in container " } else { "" },
            iteration,
        ));

        if let Err(e) = self.ensure_worker(config) {
            log_error(&e);
            return AgentRunStats {
                success: false,
                wall_secs: wall_start.elapsed().as_secs_f64(),
                stream: stream::StreamSummary::default(),
            };
        }

        let log_path = config
            .log_dir
            .as_ref()
            .map(|dir| PathBuf::from(dir).join(format!("{}-{}.jsonl", log_prefix, iteration)));

        let worker = self.worker.as_mut().expect("worker just ensured");
        signal::set_child_pid(-(worker.child_id() as i32));

        let prompt_id = worker.next_id("prompt");
        if let Err(e) = worker.send_prompt(&prompt_id, prompt) {
            log_error(&format!("failed to send OMP RPC prompt: {}", e));
            signal::set_child_pid(0);
            return AgentRunStats {
                success: false,
                wall_secs: wall_start.elapsed().as_secs_f64(),
                stream: stream::StreamSummary::default(),
            };
        }

        let mut formatter = stream_omp::OmpStreamProcessor::new(log_path.as_deref(), prior_total);
        let mut pending: HashMap<String, PendingRpc> = HashMap::new();
        pending.insert(prompt_id, PendingRpc::Prompt);
        let mut success = true;
        let mut saw_agent_end = false;

        while !signal::interrupted() {
            drain_control(
                self.control.as_ref(),
                &mut self.queued,
                &mut self.pause_explicit,
                &mut self.shutdown_requested,
            );

            if self.paused {
                if self.shutdown_requested {
                    // Stop wins over pause: thaw the worker so it can accept
                    // the graceful shutdown steering.
                    self.resume_worker();
                } else {
                    let backpressure_drained = !self.pause_backpressure
                        || self
                            .summary
                            .as_ref()
                            .is_none_or(|summary| summary.pending() == 0);
                    if backpressure_drained && !self.pause_explicit {
                        self.pause_backpressure = false;
                        self.resume_worker();
                    } else if let Ok(Some(status)) =
                        self.worker.as_mut().expect("worker").try_wait()
                    {
                        log_error(&format!("OMP RPC process exited while paused: {}", status));
                        success = false;
                        break;
                    } else {
                        thread::sleep(RPC_EVENT_POLL);
                        continue;
                    }
                }
            }

            if let Some(summary) = self.summary.as_ref()
                && summary.enabled()
            {
                if summary.pending() == 0 {
                    self.pause_backpressure = false;
                } else if summary.pending() >= config.summary_queue_limit as usize {
                    if !self.pause_backpressure {
                        log(&format!(
                            "summary backlog reached {}; pausing at the next block boundary",
                            summary.pending()
                        ));
                    }
                    self.pause_backpressure = true;
                }
            }

            deliver_queued(
                self.worker.as_mut().expect("worker"),
                &mut self.queued,
                &mut pending,
                &mut self.shutdown_requested,
            );

            match self
                .worker
                .as_mut()
                .expect("worker")
                .recv_line(RPC_EVENT_POLL)
            {
                Ok(Some(line)) => {
                    if handle_rpc_response(&line, &mut pending) == RpcResponseOutcome::Failure {
                        success = false;
                    }
                    let is_agent_end = formatter.process_line(&line);
                    if let Some(summary) = self.summary.as_mut() {
                        summary.observe(&line);
                    }
                    let at_boundary = self.summary.as_ref().is_none_or(SummaryEngine::at_boundary);
                    if !is_agent_end
                        && at_boundary
                        && (self.pause_explicit || self.pause_backpressure)
                    {
                        self.pause_worker();
                        continue;
                    }
                    if is_agent_end {
                        saw_agent_end = true;
                        break;
                    }
                }
                Ok(None) => {
                    if let Ok(Some(status)) = self.worker.as_mut().expect("worker").try_wait() {
                        log_error(&format!("OMP RPC process exited early: {}", status));
                        success = false;
                        break;
                    }
                    let at_boundary = self.summary.as_ref().is_none_or(SummaryEngine::at_boundary);
                    if at_boundary && (self.pause_explicit || self.pause_backpressure) {
                        self.pause_worker();
                        continue;
                    }
                }
                Err(e) => {
                    log_error(&format!("failed to read OMP RPC stream: {}", e));
                    success = false;
                    break;
                }
            }
        }

        signal::set_child_pid(0);
        let summary = formatter.finish();
        if signal::interrupted() || !saw_agent_end {
            success = false;
        }

        log(&format!(
            "{} RPC turn {} ({})",
            label,
            iteration,
            if success { "success" } else { "failure" }
        ));
        AgentRunStats {
            success,
            wall_secs: wall_start.elapsed().as_secs_f64(),
            stream: summary,
        }
    }

    fn pause_worker(&mut self) {
        if self.paused {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let pgid = -(worker.child_id() as i32);
        if signal::stop_child(pgid) {
            self.paused = true;
            log("worker paused at block boundary");
        } else {
            log_error(&format!(
                "failed to pause worker process {}",
                worker.child_id()
            ));
        }
    }

    fn resume_worker(&mut self) {
        if !self.paused {
            return;
        }
        if let Some(backup_dir) = self.backup_dir.as_deref() {
            crate::snapshot_files_to(backup_dir, &self.protected);
        }
        let Some(worker) = self.worker.as_ref() else {
            self.paused = false;
            return;
        };
        let pgid = -(worker.child_id() as i32);
        if signal::continue_child(pgid) {
            self.paused = false;
            log("worker resumed; protected files re-snapshotted");
        } else {
            log_error(&format!(
                "failed to resume worker process {}",
                worker.child_id()
            ));
        }
    }

    fn ensure_worker(&mut self, config: &Config) -> Result<(), String> {
        if self.worker.is_none() {
            self.worker = Some(OmpRpcWorker::start(config)?);
        }
        Ok(())
    }

    fn stop_worker(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }
}

impl Drop for RunSupervisor {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

fn drain_control(
    control: Option<&ControlServer>,
    queued: &mut VecDeque<Injection>,
    pause_explicit: &mut bool,
    shutdown_requested: &mut bool,
) {
    let Some(control) = control else {
        return;
    };
    while let Some(command) = control.try_recv() {
        match command {
            ControlCommand::Inject(injection) => {
                log(&format!(
                    "queued {} steering {}",
                    injection.kind.as_str(),
                    injection.id
                ));
                queued.push_back(injection);
            }
            ControlCommand::Pause => {
                *pause_explicit = true;
                log("pause requested; will pause at the next block boundary");
            }
            ControlCommand::Resume => {
                *pause_explicit = false;
                log("resume requested");
            }
            ControlCommand::Stop => {
                *shutdown_requested = true;
                *pause_explicit = false;
                let millis = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                queued.push_back(Injection {
                    id: format!("stop-{}", millis),
                    kind: InjectionKind::Shutdown,
                    message: "Stop the run gracefully and report the final result.".to_string(),
                    reply: None,
                });
                log("stop requested; delivering graceful shutdown");
            }
        }
    }
}

fn deliver_queued(
    worker: &mut OmpRpcWorker,
    queued: &mut VecDeque<Injection>,
    pending: &mut HashMap<String, PendingRpc>,
    shutdown_requested: &mut bool,
) {
    while let Some(injection) = queued.pop_front() {
        let rpc_id = worker.next_id("steer");
        let command = if injection.kind == InjectionKind::Shutdown {
            *shutdown_requested = true;
            "steer"
        } else {
            "steer"
        };
        match worker.send_message_command(command, &rpc_id, &injection.worker_message()) {
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

#[derive(Debug, PartialEq, Eq)]
enum RpcResponseOutcome {
    Ignored,
    Success,
    Failure,
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

fn handle_rpc_response(
    line: &str,
    pending: &mut HashMap<String, PendingRpc>,
) -> RpcResponseOutcome {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return RpcResponseOutcome::Ignored;
    };
    if value.get("type").and_then(Value::as_str) != Some("response") {
        return RpcResponseOutcome::Ignored;
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return RpcResponseOutcome::Ignored;
    };
    let Some(pending_rpc) = pending.remove(id) else {
        return RpcResponseOutcome::Ignored;
    };
    let success = value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if success {
        match pending_rpc {
            PendingRpc::Prompt => {}
            PendingRpc::Injection { id, kind, reply } => {
                log(&format!(
                    "delivered {} steering {} to OMP RPC",
                    kind.as_str(),
                    id
                ));
                if let Some(reply) = reply {
                    let _ = reply.send(DeliveryStatus::Delivered);
                }
            }
        }
        RpcResponseOutcome::Success
    } else {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown RPC error");
        match pending_rpc {
            PendingRpc::Prompt => log_error(&format!("OMP RPC rejected prompt: {}", error)),
            PendingRpc::Injection { id, kind, reply } => {
                log_error(&format!(
                    "OMP RPC rejected {} steering {}: {}",
                    kind.as_str(),
                    id,
                    error
                ));
                if let Some(reply) = reply {
                    let _ = reply.send(DeliveryStatus::Failed(error.to_string()));
                }
            }
        }
        RpcResponseOutcome::Failure
    }
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
        let mut cmd = build_rpc_command(config);
        // Own process group: pause can freeze tool children along with the
        // RPC process, and Ctrl+C can terminate the whole tree.
        cmd.process_group(0);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                let bin = if config.image.is_some() {
                    "docker"
                } else {
                    "omp"
                };
                format!("failed to spawn '{}': {}", bin, e)
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
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
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

    fn child_id(&self) -> u32 {
        self.child.id()
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
            let timeout = remaining.min(RPC_EVENT_POLL);
            match self.lines.recv_timeout(timeout) {
                Ok(line) => {
                    if serde_json::from_str::<Value>(&line)
                        .ok()
                        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
                        .as_deref()
                        == Some("ready")
                    {
                        return Ok(());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("OMP RPC stream closed before ready".to_string());
                }
            }
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("yoke-{}-{}", prefix, self.next_id)
    }

    fn send_prompt(&mut self, id: &str, message: &str) -> io::Result<()> {
        self.send_message_command("prompt", id, message)
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

    fn recv_line(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Ok(Some(line)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "OMP RPC stream closed",
            )),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn stop(&mut self) {
        signal::terminate_group(-(self.child.id() as i32));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for OmpRpcWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_rpc_args(config: &Config) -> Vec<String> {
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        "--auto-approve".to_string(),
    ];
    if let Some(model) = config.model.as_deref() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(thinking) = config.thinking {
        args.push("--thinking".to_string());
        args.push(thinking.as_omp_arg().to_string());
    }
    args
}

fn build_rpc_command(config: &Config) -> Command {
    let args = build_rpc_args(config);
    match &config.image {
        Some(image) => crate::build_docker_omp_command(image, &args),
        None => {
            let mut c = Command::new("omp");
            c.args(&args);
            c
        }
    }
}

pub(crate) fn send_inject_command(
    path: impl AsRef<Path>,
    kind: InjectionKind,
    message: &str,
) -> Result<Value, String> {
    send_control_command(
        path,
        json!({
            "type": "inject",
            "kind": kind.as_str(),
            "message": message,
        }),
    )
}

pub(crate) fn send_pause_command(path: impl AsRef<Path>) -> Result<Value, String> {
    send_control_command(path, json!({"type": "pause"}))
}

pub(crate) fn send_resume_command(path: impl AsRef<Path>) -> Result<Value, String> {
    send_control_command(path, json!({"type": "resume"}))
}

pub(crate) fn send_stop_command(path: impl AsRef<Path>) -> Result<Value, String> {
    send_control_command(path, json!({"type": "stop"}))
}

fn send_control_command(path: impl AsRef<Path>, request: Value) -> Result<Value, String> {
    let path = path.as_ref();
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

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("failed to read control response: {}", e))?;
    if line.trim().is_empty() {
        return Err("active yoke run closed the control response".to_string());
    }
    serde_json::from_str::<Value>(line.trim())
        .map_err(|e| format!("invalid control response JSON: {}", e))
}
