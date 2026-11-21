use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn sigint_handler(_sig: i32) {
    if INTERRUPTED.swap(true, Ordering::Relaxed) {
        // Second Ctrl+C — force exit immediately.
        // _exit is async-signal-safe (unlike std::process::exit).
        _exit(130);
    }
    // Kill the child to close the pipe and unblock reader.lines().
    // Rust's BufReader retries on EINTR internally, so the only way
    // to break out of the blocking read is to close the write end.
    let pid = CHILD_PID.load(Ordering::Relaxed);
    if pid > 0 {
        kill(pid, 9); // SIGKILL — child already got SIGINT from the terminal
    }
}

unsafe extern "C" {
    safe fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    safe fn kill(pid: i32, sig: i32) -> i32;
    safe fn _exit(status: i32) -> !;
}

pub fn install() {
    signal(2, sigint_handler);
}

pub fn set_child_pid(pid: i32) {
    CHILD_PID.store(pid, Ordering::Relaxed);
}

/// Freeze the child process in place. SIGSTOP is uncatchable, so the worker
/// resumes exactly where it stopped and keeps its session and fds.
pub fn stop_child(pid: i32) -> bool {
    pid != 0 && kill(pid, 19) == 0
}

/// Resume a child frozen by [`stop_child`].
pub fn continue_child(pid: i32) -> bool {
    pid != 0 && kill(pid, 18) == 0
}

/// SIGKILL a worker's process group so tool children do not outlive it.
pub fn terminate_group(pid: i32) {
    if pid != 0 {
        kill(pid, 9);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}
