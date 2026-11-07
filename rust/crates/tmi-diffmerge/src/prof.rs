//! `TMI_DIFF_PROFILE=1`: where a diff spends its time, on stderr at the end
//! (`report`). Each stage adds up the wall time of its calls; calls on
//! parallel threads overlap, so a stage's sum can exceed the elapsed time.
//! Without the variable nothing is measured.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TMI_DIFF_PROFILE").is_some_and(|v| !v.is_empty() && v != "0"))
}

struct Table {
    start: Instant,
    rows: Vec<(&'static str, Duration, u64)>,
}

fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Table { start: Instant::now(), rows: Vec::new() }))
}

fn add(stage: &'static str, d: Duration) {
    let mut t = table().lock().unwrap();
    match t.rows.iter_mut().find(|r| r.0 == stage) {
        Some(r) => {
            r.1 += d;
            r.2 += 1;
        }
        None => t.rows.push((stage, d, 1)),
    }
}

/// `f()`, its wall time added to `stage`.
pub fn time<T>(stage: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let _ = table();
    let t0 = Instant::now();
    let out = f();
    add(stage, t0.elapsed());
    out
}

/// Prints the stages (first use order) and the time since the first one.
pub fn report() {
    if !enabled() {
        return;
    }
    let t = table().lock().unwrap();
    eprintln!("tmiki diff profile ({:.3}s since the first stage):", t.start.elapsed().as_secs_f64());
    for (stage, d, n) in &t.rows {
        eprintln!("  {stage:<34} {:>9.3}s  x{n}", d.as_secs_f64());
    }
}
