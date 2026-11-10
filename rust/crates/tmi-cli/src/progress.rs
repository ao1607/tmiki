//! cli.py `Progress`: phase reporting on stderr for long operations.
//!
//! On a terminal (stderr and stdout both), one spinner line "<frame> tmi:
//! <phase> (<elapsed>s)", redrawn in place and erased when done. With
//! `TMI_PROGRESS=1` otherwise, one line per phase change (at most one a
//! second within a phase) and a closing per-phase time breakdown.
//! Otherwise, and always with `TMI_PROGRESS=0`, silent.
//!
//! The spinner is drawn as it happens. The lines are held, numbered in one
//! sequence with what `Out` holds, and `Out` writes them in that order with
//! its own output (lines come out where they were printed).

use crate::out::{self, Encoding, Stream};
use indexmap::IndexMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BRAILLE_FRAMES: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
const ASCII_FRAMES: &str = "|/-\\";
const INTERVAL: Duration = Duration::from_millis(80);

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The place of a line in what this process prints (`Out` and the progress
/// lines share it).
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Progress lines not written yet: (sequence number, text with "\n").
static HELD: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<(u64, String)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The held lines, for `Out` to write.
pub fn take_held() -> Vec<(u64, String)> {
    std::mem::take(&mut *held())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tty,
    Lines,
}

struct State {
    message: String,
    last_len: usize,
    frames: Vec<char>,
    frame: usize,
    phase: Option<String>,
    phase_started: Instant,
    phase_times: IndexMap<String, f64>,
    last_line_phase: Option<String>,
    last_line_at: Option<Instant>,
    done: bool,
    /// Since when a question on the terminal holds the spinner (issue #48):
    /// nothing is drawn, and the time does not count.
    paused_since: Option<Instant>,
    /// The time spent in such questions so far.
    paused_total: Duration,
}

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The spinner on the terminal now, if any (`Out` erases it before it
/// writes).
static ACTIVE: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

pub struct Progress {
    mode: Option<Mode>,
    started: Instant,
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// `_phase_key(msg)`: its leading lowercase words, at most two.
fn phase_key(msg: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for word in msg.split_whitespace() {
        if words.len() == 2 || word.is_empty() || !word.bytes().all(|b| b.is_ascii_lowercase()) {
            break;
        }
        words.push(word);
    }
    if words.is_empty() {
        msg.to_string()
    } else {
        words.join(" ")
    }
}

/// The mode `Progress()` picks: TMI_PROGRESS=0 is silent, a terminal
/// (stderr and stdout) spins, TMI_PROGRESS=1 writes lines.
pub fn mode_from_env() -> Option<&'static str> {
    let env = std::env::var("TMI_PROGRESS").unwrap_or_default().trim().to_lowercase();
    if ["0", "false", "no", "off"].contains(&env.as_str()) {
        None
    } else if out::is_tty(Stream::Stderr) && out::is_tty(Stream::Stdout) {
        Some("tty")
    } else if ["1", "true", "yes", "on"].contains(&env.as_str()) {
        Some("lines")
    } else {
        None
    }
}

fn write_err(text: &str) {
    let mut err = std::io::stderr().lock();
    // text mode and the stream's encoding, as Out writes it
    let _ = err.write_all(&out::text_bytes(Stream::Stderr, text));
    let _ = err.flush();
}

/// Erase the spinner line before other output (cli.py `progress.clear()`).
pub fn clear_active() {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(shared) = active {
        shared.lock().erase();
    }
}

/// Before a question on the terminal (`plugin.trust=prompt`, the consent to
/// write through a plugin; issue #48): the spinner line is erased and not
/// drawn again, and its clock stops, until [`resume_active`].
pub fn pause_active() {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(shared) = active {
        let mut s = shared.lock();
        if s.paused_since.is_none() {
            s.paused_since = Some(Instant::now());
        }
        s.erase_line();
    }
}

/// After the question: the spinner goes on where it was.
pub fn resume_active() {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(shared) = active {
        let mut s = shared.lock();
        if let Some(at) = s.paused_since.take() {
            let spent = at.elapsed();
            s.paused_total += spent;
            s.phase_started += spent;
        }
    }
}

impl State {
    fn enter_phase(&mut self, key: &str, now: Instant) {
        if self.phase.as_deref() == Some(key) {
            return;
        }
        if let Some(prev) = self.phase.take() {
            let spent = (now - self.phase_started).as_secs_f64();
            *self.phase_times.entry(prev).or_insert(0.0) += spent;
        }
        self.phase = Some(key.to_string());
        self.phase_started = now;
    }

    fn draw(&mut self, started: Instant) {
        if self.paused_since.is_some() {
            return;
        }
        let frame = self.frames[self.frame % self.frames.len()];
        self.frame += 1;
        let elapsed = started.elapsed().saturating_sub(self.paused_total).as_secs_f64();
        let text = format!("{frame} tmiki: {} ({elapsed:.1}s)", self.message);
        let len = text.chars().count();
        let pad = " ".repeat(self.last_len.saturating_sub(len));
        write_err(&format!("\r{text}{pad}"));
        self.last_len = len;
    }

    fn erase(&mut self) {
        self.erase_line();
        self.message.clear();
    }

    /// The spinner line blanked; the phase stays (drawn again later).
    fn erase_line(&mut self) {
        if self.last_len > 0 {
            write_err(&format!("\r{}\r", " ".repeat(self.last_len)));
            self.last_len = 0;
        }
    }
}

impl Progress {
    pub fn new() -> Progress {
        let mode = match mode_from_env() {
            Some("tty") => Some(Mode::Tty),
            Some("lines") => Some(Mode::Lines),
            _ => None,
        };
        let frames = if out::encoding(Stream::Stderr) == Encoding::Utf8 { BRAILLE_FRAMES } else { ASCII_FRAMES };
        let started = Instant::now();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                message: String::new(),
                last_len: 0,
                frames: frames.chars().collect(),
                frame: 0,
                phase: None,
                phase_started: started,
                phase_times: IndexMap::new(),
                last_line_phase: None,
                last_line_at: None,
                done: false,
                paused_since: None,
                paused_total: Duration::ZERO,
            }),
            stop: AtomicBool::new(false),
        });
        let mut thread = None;
        if mode == Some(Mode::Tty) {
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared.clone());
            let shared = shared.clone();
            thread = Some(std::thread::spawn(move || loop {
                std::thread::sleep(INTERVAL);
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                let mut s = shared.lock();
                if !s.message.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                    s.draw(started);
                }
            }));
        }
        Progress { mode, started, shared, thread: Mutex::new(thread) }
    }

    /// `progress(msg)`.
    pub fn report(&self, msg: &str) {
        let Some(mode) = self.mode else { return };
        let mut s = self.shared.lock();
        if s.done {
            return;
        }
        let now = Instant::now();
        let key = phase_key(msg);
        s.enter_phase(&key, now);
        if mode == Mode::Tty {
            s.message = msg.to_string();
            s.draw(self.started);
            return;
        }
        let due = s.last_line_at.is_none_or(|at| (now - at).as_secs_f64() >= 1.0);
        if s.last_line_phase.as_deref() != Some(key.as_str()) || due {
            held().push((next_seq(), format!("tmiki: [{:6.1}s] {msg}\n", (now - self.started).as_secs_f64())));
            s.last_line_phase = Some(key);
            s.last_line_at = Some(now);
        }
    }

    /// `progress.clear()`: erase the spinner line (terminal only).
    pub fn clear(&self) {
        if self.mode == Some(Mode::Tty) {
            self.shared.lock().erase();
        }
    }

    /// `progress.done()` (once; dropping it does it too, as Python's
    /// `finally:` does).
    pub fn done(&self) {
        match self.mode {
            None => {}
            Some(Mode::Lines) => {
                let mut s = self.shared.lock();
                if s.done {
                    return;
                }
                s.done = true;
                if s.phase.is_none() {
                    return;
                }
                let now = Instant::now();
                s.enter_phase("", now);
                s.phase = None;
                let parts: Vec<String> =
                    s.phase_times.iter().filter(|(k, _)| !k.is_empty()).map(|(k, v)| format!("{k} {v:.1}s")).collect();
                held().push((next_seq(), format!("tmiki: done in {:.1}s ({})\n", (now - self.started).as_secs_f64(), parts.join(", "))));
            }
            Some(Mode::Tty) => {
                self.shared.stop.store(true, Ordering::SeqCst);
                if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = t.join();
                }
                let mut s = self.shared.lock();
                s.done = true;
                s.erase();
                let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
                if active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &self.shared)) {
                    *active = None;
                }
            }
        }
    }
}

/// cli.py `_with_progress(message, fn)`: `f()` as one phase.
pub fn with_progress<T>(message: &str, f: impl FnOnce() -> T) -> T {
    let progress = Progress::new();
    progress.report(message);
    let out = f();
    progress.done();
    out
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases() {
        assert_eq!(phase_key("decoding layers 3/40"), "decoding layers");
        assert_eq!(phase_key("copying object [1/9] abc"), "copying object");
        assert_eq!(phase_key("fetching 3 remote objects via HTTP..."), "fetching");
        assert_eq!(phase_key("Merge x"), "Merge x");
        assert_eq!(phase_key("switching to main"), "switching to");
    }

    /// Issue #48: while a question waits, the spinner draws nothing and its
    /// clock stops; afterwards it goes on.
    #[test]
    fn a_question_holds_the_spinner() {
        let started = Instant::now();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                message: "parsing a.ora".into(),
                last_len: 0,
                frames: ASCII_FRAMES.chars().collect(),
                frame: 0,
                phase: Some("parsing".into()),
                phase_started: started,
                phase_times: IndexMap::new(),
                last_line_phase: None,
                last_line_at: None,
                done: false,
                paused_since: None,
                paused_total: Duration::ZERO,
            }),
            stop: AtomicBool::new(false),
        });
        *ACTIVE.lock().unwrap() = Some(shared.clone());
        pause_active();
        shared.lock().draw(started);
        assert_eq!(shared.lock().frame, 0, "nothing is drawn while paused");
        std::thread::sleep(Duration::from_millis(30));
        resume_active();
        *ACTIVE.lock().unwrap() = None;
        let s = shared.lock();
        assert!(s.paused_since.is_none());
        assert!(s.paused_total >= Duration::from_millis(30));
        assert!(s.phase_started >= started + Duration::from_millis(30), "the phase's time leaves the wait out");
        assert_eq!(s.message, "parsing a.ora", "the phase is drawn again after the question");
    }
}
