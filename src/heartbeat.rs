//! Pulse Heartbeat — background tick that fires every 500ms and emits
//! structured events to registered listeners.
//!
//! The heartbeat is stoppable and restartable without losing listener
//! subscriptions. Listeners receive a `HeartbeatEvent` on each tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Structured event emitted on each heartbeat tick.
#[derive(Debug, Clone)]
pub struct HeartbeatEvent {
    /// Monotonically increasing tick number (starts at 1).
    pub tick: u64,
    /// Wall-clock instant when this tick fired.
    pub timestamp: Instant,
    /// Elapsed time since the heartbeat was (re)started.
    pub elapsed: Duration,
}

/// Callback type for heartbeat listeners.
pub type Listener = Box<dyn Fn(&HeartbeatEvent) + Send + 'static>;

/// A named listener entry so subscriptions survive stop/start cycles.
struct ListenerEntry {
    name: String,
    callback: Listener,
}

/// Background heartbeat that ticks at a fixed interval and notifies listeners.
///
/// Listeners are registered once and persist across stop/start cycles.
/// The heartbeat thread is spawned on `start()` and joined on `stop()`.
pub struct Heartbeat {
    interval: Duration,
    running: Arc<AtomicBool>,
    listeners: Arc<Mutex<Vec<ListenerEntry>>>,
    handle: Option<thread::JoinHandle<()>>,
    tick_count: Arc<Mutex<u64>>,
}

impl Heartbeat {
    /// Create a new heartbeat with the given tick interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            running: Arc::new(AtomicBool::new(false)),
            listeners: Arc::new(Mutex::new(Vec::new())),
            handle: None,
            tick_count: Arc::new(Mutex::new(0)),
        }
    }

    /// Create a heartbeat with the default 500ms interval.
    pub fn default_pulse() -> Self {
        Self::new(Duration::from_millis(500))
    }

    /// Register a named listener. The listener persists across stop/start cycles.
    pub fn on_tick(&self, name: impl Into<String>, callback: impl Fn(&HeartbeatEvent) + Send + 'static) {
        let mut listeners = self.listeners.lock().unwrap();
        let name = name.into();
        // Replace existing listener with the same name
        listeners.retain(|e| e.name != name);
        listeners.push(ListenerEntry {
            name,
            callback: Box::new(callback),
        });
    }

    /// Remove a listener by name.
    pub fn remove_listener(&self, name: &str) {
        let mut listeners = self.listeners.lock().unwrap();
        listeners.retain(|e| e.name != name);
    }

    /// Start the heartbeat. If already running, this is a no-op.
    pub fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }

        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let listeners = Arc::clone(&self.listeners);
        let tick_count = Arc::clone(&self.tick_count);
        let interval = self.interval;

        let handle = thread::spawn(move || {
            let start_time = Instant::now();

            while running.load(Ordering::SeqCst) {
                thread::sleep(interval);

                if !running.load(Ordering::SeqCst) {
                    break;
                }

                let tick = {
                    let mut count = tick_count.lock().unwrap();
                    *count += 1;
                    *count
                };

                let event = HeartbeatEvent {
                    tick,
                    timestamp: Instant::now(),
                    elapsed: start_time.elapsed(),
                };

                let listeners = listeners.lock().unwrap();
                for entry in listeners.iter() {
                    (entry.callback)(&event);
                }
            }
        });

        self.handle = Some(handle);
    }

    /// Stop the heartbeat and join the background thread.
    /// Listener subscriptions are preserved for a subsequent `start()`.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Whether the heartbeat is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Total number of ticks emitted (across all start/stop cycles).
    pub fn total_ticks(&self) -> u64 {
        *self.tick_count.lock().unwrap()
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}
