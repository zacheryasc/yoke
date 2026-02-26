use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn sigint_handler(_sig: i32) {
    INTERRUPTED.store(true, Ordering::Relaxed);
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
}

pub fn install() {
    signal(2, sigint_handler);
}

pub fn set_child_pid(pid: i32) {
    CHILD_PID.store(pid, Ordering::Relaxed);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}
