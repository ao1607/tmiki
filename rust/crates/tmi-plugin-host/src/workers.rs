//! The threads wasm runs on (PLUGINS.md 6.1).
//!
//! Every plugin call runs on one of these, never on the caller's thread:
//! - wasm needs a deep native stack (`max_wasm_stack` is 8 MiB; a rayon
//!   worker has 2 MiB, Windows' main thread 1 MiB), and an overflow of the
//!   host stack would end the process rather than trap;
//! - a rayon thread that waits here waits on a thread outside rayon, which
//!   needs nothing from rayon to finish (a worker never calls rayon, nor
//!   another worker), so the shared pool cannot deadlock on plugin calls
//!   (the P2 lesson: nothing waits on rayon while holding what rayon needs);
//! - their number is the most instances alive at once (the pooling
//!   allocator has that many slots).

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct Workers {
    tx: Mutex<Sender<Job>>,
    count: usize,
}

impl Workers {
    /// `count` threads of `stack` bytes each.
    pub fn new(count: usize, stack: usize) -> Workers {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..count.max(1) {
            let rx: Arc<Mutex<Receiver<Job>>> = Arc::clone(&rx);
            std::thread::Builder::new()
                .name(format!("tmi-plugin-{i}"))
                .stack_size(stack)
                .spawn(move || loop {
                    let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                        Ok(job) => job,
                        Err(_) => return, // the pool is gone
                    };
                    job();
                })
                .expect("spawn a plugin worker");
        }
        Workers { tx: Mutex::new(tx), count: count.max(1) }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Runs `f` on a worker and waits for it. A panic in `f` is resumed
    /// here.
    pub fn run<'a, R: Send + 'a>(&self, f: impl FnOnce() -> R + Send + 'a) -> R {
        let (rtx, rrx) = sync_channel(1);
        let job: Box<dyn FnOnce() + Send + 'a> = Box::new(move || {
            let r = catch_unwind(AssertUnwindSafe(f));
            let _ = rtx.send(r);
        });
        // SAFETY: the job may borrow data of lifetime 'a. This function
        // does not return before the job has run to its end and sent its
        // result (the send is the job's last use of anything it captured),
        // or before the job has been dropped unrun (then `recv` fails and
        // this panics), so nothing borrowed outlives its borrow.
        let job: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + 'a>, Job>(job) };
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job).expect("the plugin workers are running");
        match rrx.recv() {
            Ok(Ok(r)) => r,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("a plugin worker dropped its job"),
        }
    }
}
