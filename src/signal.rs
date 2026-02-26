use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_sig: i32) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

unsafe extern "C" {
    safe fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

pub fn install() {
    signal(2, sigint_handler); // SIGINT = 2
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}
