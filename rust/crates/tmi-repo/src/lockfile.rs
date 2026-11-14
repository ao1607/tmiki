//! tmi/lockfile.py: Git-style lock files (issues #35, #36).
//!
//! Every write to the index or to a ref goes through a lock file created with
//! `create_new` (O_CREAT | O_EXCL):
//!
//! * `<repo>/index.lock` is taken by a command before it reads the index it
//!   will change and kept until the command ends ([`hold`]; `Repo::lock_index`).
//! * `<ref path>.lock` (also `HEAD.lock`) is taken for one ref update
//!   ([`write_locked`]): the ref is re-read under the lock and compared with
//!   the value the caller expects (compare-and-swap), the new value is written
//!   into the lock file, and the lock is renamed over the ref.
//!
//! A lock that exists stops the command at once with [`lock_message`], no
//! waiting and nothing written. The words are Python's, character for
//! character.
//!
//! Windows: refs are read without a lock, and renaming the lock over a ref
//! that a reader has open fails while the handle stays open (lockfile.py's
//! docstring); the rename waits that out ([`fsio::rename_over`], at most
//! [`fsio::RENAME_RETRY`]) and then stops with [`busy_message`]. Rust's own
//! reads pass FILE_SHARE_DELETE, so they never fail on a rename in flight.

use crate::error::{RepoError, Result};
use crate::fsio;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What a ref update must find under its lock.
#[derive(Clone, Copy, Debug)]
pub enum Expect<'a> {
    /// Do not compare.
    Any,
    /// The ref must be this value (None: it must not exist).
    Is(Option<&'a str>),
}

/// `lock_message(display)`.
pub fn lock_message(display: &str) -> String {
    format!(
        "unable to create '{display}': File exists. Another tmiki process seems to be running in this \
         repository. If no other tmiki process is running, a previous one probably crashed: \
         remove the file and try again."
    )
}

/// `busy_message(display)`: the ref's file stayed open in another process
/// for all of [`fsio::RENAME_RETRY`] (Windows).
pub fn busy_message(display: &str) -> String {
    format!(
        "unable to update '{display}': another program keeps the file open (another tmiki process \
         reading it, a virus scanner, a backup or sync tool); nothing was written. Try again."
    )
}

/// `RefMovedError`'s text.
pub fn ref_moved_message(name: &str, expected: Option<&str>, actual: Option<&str>) -> String {
    let short = |v: Option<&str>| match v {
        Some(v) if !v.is_empty() => v.chars().take(12).collect::<String>(),
        _ => "missing".to_string(),
    };
    format!(
        "{name} was changed by another tmiki process while this command was running \
         (it was {}, it is now {}); nothing was written",
        short(expected),
        short(actual)
    )
}

/// How the message names a lock file: `.tmi/index.lock`.
pub fn display(repo_dir: &Path, path: &Path) -> String {
    let base = repo_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rel = path.strip_prefix(repo_dir).unwrap_or(path);
    let rel: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    format!("{base}/{}", rel.join("/"))
}

/// Create `path` exclusively; the message error if it exists.
pub fn acquire(path: &Path, display: &str) -> Result<std::fs::File> {
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(RepoError::runtime(lock_message(display))),
        // Windows: the file exists and its owner is deleting it right now.
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied && path.parent().is_some_and(Path::is_dir) => {
            Err(RepoError::runtime(lock_message(display)))
        }
        Err(e) => Err(fsio::io_err(path, e)),
    }
}

/// Write `data` to `path` through `<path>.lock`. With [`Expect::Is`] the
/// current content (stripped; None when missing or empty) is compared first:
/// `Ok(Some(actual))` means it differed and nothing was written.
pub fn write_locked(
    repo_dir: &Path,
    path: &Path,
    data: &[u8],
    expect: Expect<'_>,
) -> Result<Option<Option<String>>> {
    if let Some(parent) = path.parent() {
        fsio::mkdir_p(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lock = path.with_file_name(format!("{name}.lock"));
    let mut file = acquire(&lock, &display(repo_dir, &lock))?;
    let result = (|| -> Result<Option<Option<String>>> {
        if let Expect::Is(want) = expect {
            let actual = if path.is_file() {
                crate::repo::read_text(path)?
                    .map(|t| crate::repo::py_strip(&t).to_string())
                    .filter(|t| !t.is_empty())
            } else {
                None
            };
            if actual.as_deref() != want {
                return Ok(Some(actual));
            }
        }
        fsio::mark_mutated();
        file.write_all(data).and_then(|()| file.flush()).map_err(|e| fsio::io_err(&lock, e))?;
        Ok(None)
    })();
    drop(file);
    match &result {
        Ok(None) => {
            if let Err(e) = fsio::rename_over(&lock, path) {
                let _ = std::fs::remove_file(&lock);
                if fsio::is_sharing_error(&e) && !path.is_dir() {
                    return Err(RepoError::runtime(busy_message(&display(repo_dir, path))));
                }
                return Err(fsio::io_err(path, e));
            }
        }
        _ => {
            let _ = std::fs::remove_file(&lock);
        }
    }
    result
}

/// `Path.unlink(missing_ok=True)` under `<path>.lock`, with the same
/// compare as [`write_locked`].
pub fn delete_locked(repo_dir: &Path, path: &Path, expect: Expect<'_>) -> Result<Option<Option<String>>> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lock = path.with_file_name(format!("{name}.lock"));
    drop(acquire(&lock, &display(repo_dir, &lock))?);
    let result = (|| -> Result<Option<Option<String>>> {
        if let Expect::Is(want) = expect {
            let actual = if path.is_file() {
                crate::repo::read_text(path)?
                    .map(|t| crate::repo::py_strip(&t).to_string())
                    .filter(|t| !t.is_empty())
            } else {
                None
            };
            if actual.as_deref() != want {
                return Ok(Some(actual));
            }
        }
        fsio::unlink_missing_ok(path)?;
        Ok(None)
    })();
    let _ = std::fs::remove_file(&lock);
    result
}

// -------- locks held for the rest of the process --------

static HELD: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<(PathBuf, String)>> {
    HELD.lock().unwrap_or_else(|p| p.into_inner())
}

/// Take the lock `path` until [`release`] / [`release_all`]; false when this
/// process already holds it.
pub fn hold(path: &Path, display: &str) -> Result<bool> {
    if held().iter().any(|(p, _)| p == path) {
        return Ok(false);
    }
    let mut file = acquire(path, display)?;
    let _ = writeln!(file, "{}", std::process::id());
    drop(file);
    held().push((path.to_path_buf(), display.to_string()));
    Ok(true)
}

pub fn release(path: &Path) {
    let mut g = held();
    if let Some(i) = g.iter().position(|(p, _)| p == path) {
        g.remove(i);
        drop(g);
        let _ = std::fs::remove_file(path);
    }
}

/// Give up the locks while a hook runs (it may run tmiki); see [`resume`].
pub fn suspend() -> Vec<(PathBuf, String)> {
    let all: Vec<(PathBuf, String)> = held().clone();
    for (p, _) in &all {
        release(p);
    }
    all
}

pub fn resume(all: Vec<(PathBuf, String)>) -> Result<()> {
    for (p, d) in all {
        hold(&p, &d)?;
    }
    Ok(())
}

/// Remove every lock this process holds (end of the command, before handing
/// it to Python, Ctrl+C).
pub fn release_all() {
    let all: Vec<PathBuf> = match HELD.try_lock() {
        Ok(mut g) => g.drain(..).map(|(p, _)| p).collect(),
        Err(_) => return,
    };
    for p in all {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tmi-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".tmi")).unwrap();
        d.join(".tmi")
    }

    #[test]
    fn exclusive_hold() {
        let dir = scratch("hold");
        let lock = dir.join("index.lock");
        assert!(hold(&lock, ".tmi/index.lock").unwrap());
        assert!(!hold(&lock, ".tmi/index.lock").unwrap());
        // a second process (here: a plain create) is refused with the message
        let err = acquire(&lock, ".tmi/index.lock").unwrap_err();
        assert_eq!(err.message().unwrap(), lock_message(".tmi/index.lock"));
        release(&lock);
        assert!(!lock.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn compare_and_swap() {
        let dir = scratch("cas");
        let r = dir.join("refs").join("heads").join("main");
        assert_eq!(write_locked(&dir, &r, b"aaa\n", Expect::Is(None)).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "aaa\n");
        // the ref is not "missing" any more: refused, content kept
        assert_eq!(write_locked(&dir, &r, b"bbb\n", Expect::Is(None)).unwrap(), Some(Some("aaa".to_string())));
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "aaa\n");
        assert!(!r.with_file_name("main.lock").exists());
        assert_eq!(write_locked(&dir, &r, b"bbb\n", Expect::Is(Some("aaa"))).unwrap(), None);
        // a held ref lock refuses
        std::fs::write(r.with_file_name("main.lock"), "").unwrap();
        let err = write_locked(&dir, &r, b"ccc\n", Expect::Any).unwrap_err();
        assert!(err.message().unwrap().starts_with("unable to create '.tmi/refs/heads/main.lock'"));
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "bbb\n");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// The ref open as Python's open() opens it (no FILE_SHARE_DELETE).
    #[cfg(windows)]
    fn open_like_python(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(path).unwrap()
    }

    /// A reader's handle on the ref while the lock is renamed over it: the
    /// update waits for it (tests/test_locking.py has the same with Python).
    #[cfg(windows)]
    #[test]
    fn rename_waits_for_a_reader() {
        let dir = scratch("reader");
        let r = dir.join("refs").join("heads").join("main");
        write_locked(&dir, &r, b"aaa\n", Expect::Any).unwrap();
        let reader = open_like_python(&r);
        // without the wait, the rename fails at once
        std::fs::write(r.with_file_name("probe"), "x").unwrap();
        let bare = std::fs::rename(r.with_file_name("probe"), &r).unwrap_err();
        assert!(fsio::is_sharing_error(&bare), "{bare:?}");
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(reader);
        });
        assert_eq!(write_locked(&dir, &r, b"bbb\n", Expect::Is(Some("aaa"))).unwrap(), None);
        closer.join().unwrap();
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "bbb\n");
        assert!(!r.with_file_name("main.lock").exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// A handle that outlasts the wait: tmiki's message, the lock removed, the
    /// ref unchanged.
    #[cfg(windows)]
    #[test]
    fn rename_gives_up_with_the_message() {
        let dir = scratch("busy");
        let r = dir.join("refs").join("heads").join("main");
        write_locked(&dir, &r, b"aaa\n", Expect::Any).unwrap();
        let reader = open_like_python(&r);
        let started = std::time::Instant::now();
        let err = write_locked(&dir, &r, b"bbb\n", Expect::Is(Some("aaa"))).unwrap_err();
        assert!(started.elapsed() >= fsio::RENAME_RETRY);
        assert_eq!(err.message().unwrap(), busy_message(".tmi/refs/heads/main"));
        drop(reader);
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "aaa\n");
        assert!(!r.with_file_name("main.lock").exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// Rust's reads share FILE_SHARE_DELETE: std's rename replaces a ref
    /// they have open (Python's MoveFileEx does not; it waits, see above).
    #[cfg(windows)]
    #[test]
    fn rust_readers_do_not_block_the_rename() {
        let dir = scratch("rustreader");
        let r = dir.join("refs").join("heads").join("main");
        write_locked(&dir, &r, b"aaa\n", Expect::Any).unwrap();
        let reader = std::fs::File::open(&r).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(write_locked(&dir, &r, b"bbb\n", Expect::Any).unwrap(), None);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        drop(reader);
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "bbb\n");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
