//! File writes as tmi/repo.py makes them, and the "already wrote" mark.
//!
//! A native command that has changed anything on disk can no longer hand
//! the command to Python (Python would run it again on the changed
//! repository). Every write here sets [`mutated`]; the CLI checks it before
//! forwarding and reports an error instead.

use crate::error::{RepoError, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static MUTATED: AtomicBool = AtomicBool::new(false);

/// Whether this process has changed a file of the repository or worktree.
pub fn mutated() -> bool {
    MUTATED.load(Ordering::SeqCst)
}

pub fn mark_mutated() {
    MUTATED.store(true, Ordering::SeqCst);
}

/// After a command has put back everything it changed (only objects,
/// which are content-addressed, stay): it may go to Python again.
pub fn unmark_mutated() {
    MUTATED.store(false, Ordering::SeqCst);
}

/// An `OSError` message as `tmiki: error: {exc}` prints it is not reproduced;
/// this one names the path and the OS error.
pub fn io_err(path: &Path, e: std::io::Error) -> RepoError {
    // A full disk, in tmi's words (issue #42; cli.py `main` says the same).
    if tmi_store::store::is_no_space(&e) {
        return RepoError::runtime(format!("the disk is full while writing {}; free some space and run the command again", path.display()));
    }
    if !mutated() {
        return RepoError::forward(format!("{}: {e}", path.display()));
    }
    RepoError::runtime(format!("{}: {e}", path.display()))
}

/// repo.py `_atomic_write_bytes`: a sibling temp file renamed over `path`.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    mark_mutated();
    atomic_write_unmarked(path, data)
}

/// `atomic_write` of a file that changes nothing Python would do
/// differently afterwards (the stat cache: Python would write the same
/// entries), so the command may still be handed to Python.
pub fn atomic_write_unmarked(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut attempt = 0u32;
    let (tmp, mut file) = loop {
        let tmp = parent.join(format!("{name}.{}.tmp", temp_suffix(attempt)));
        attempt += 1;
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&tmp, e)),
        }
    };
    let result = file.write_all(data).and_then(|()| {
        drop(file);
        rename_over(&tmp, path)
    });
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(path, e));
    }
    Ok(())
}

/// lockfile.py `REPLACE_RETRY_SECONDS`.
pub const RENAME_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `e` is the error Windows gives for renaming onto a file another
/// process has open: ERROR_ACCESS_DENIED (5) or ERROR_SHARING_VIOLATION (32).
pub fn is_sharing_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32))
}

/// lockfile.py `replace`: `std::fs::rename(src, dst)`, waiting (boundedly)
/// for a Windows handle open on `dst` to close. Std's rename falls back from
/// POSIX semantics to MoveFileEx, and either fails while a process holds
/// `dst` open without FILE_SHARE_DELETE (Python's open(), another tmi
/// version, a virus scanner). Only [`is_sharing_error`] is retried, on
/// Windows only and not when `dst` is a directory, with a doubling delay from
/// 1 ms up to 100 ms for [`RENAME_RETRY`] in total; then that error is
/// returned.
pub fn rename_over(src: &Path, dst: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + RENAME_RETRY;
    let mut delay = std::time::Duration::from_millis(1);
    loop {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if is_sharing_error(&e) && !dst.is_dir() => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(e);
                }
                std::thread::sleep(delay.min(deadline - now));
                delay = (delay * 2).min(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

fn temp_suffix(attempt: u32) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u32(std::process::id());
    h.write_u32(attempt);
    let v = h.finish();
    // tempfile's names: 8 characters of [a-z0-9_]
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    (0..8).map(|i| ALPHABET[((v >> (i * 7)) % ALPHABET.len() as u64) as usize] as char).collect()
}

/// `Path.write_bytes(data)`.
pub fn write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    mark_mutated();
    std::fs::write(path, data).map_err(|e| io_err(path, e))
}

/// `Path.write_text(text, encoding="utf-8")`: "\n" becomes "\r\n" on Windows.
pub fn write_text(path: &Path, text: &str) -> Result<()> {
    write_bytes(path, &text_bytes(text))
}

/// Text as Python's text-mode files write it.
pub fn text_bytes(text: &str) -> Vec<u8> {
    if cfg!(windows) {
        text.replace('\n', "\r\n").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// `path.parent.mkdir(parents=True, exist_ok=True)` for `path`.
pub fn mkdir_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        mkdir_p(parent)?;
    }
    Ok(())
}

/// `mkdir(parents=True, exist_ok=True)`.
pub fn mkdir_p(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    mark_mutated();
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))
}

/// `Path.unlink()`.
pub fn unlink(path: &Path) -> Result<()> {
    mark_mutated();
    std::fs::remove_file(path).map_err(|e| io_err(path, e))
}

/// `Path.unlink(missing_ok=True)`.
pub fn unlink_missing_ok(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            mark_mutated();
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            mark_mutated();
            Err(io_err(path, e))
        }
    }
}

/// `shutil.rmtree(path, ignore_errors=True)`.
pub fn rmtree_ignore(path: &Path) {
    if path.exists() {
        mark_mutated();
        let _ = std::fs::remove_dir_all(path);
    }
}

/// `shutil.rmtree(path)`.
pub fn rmtree(path: &Path) -> Result<()> {
    mark_mutated();
    std::fs::remove_dir_all(path).map_err(|e| io_err(path, e))
}

/// `Path.rmdir()`; Err when it fails (not empty, missing, ...).
pub fn rmdir(path: &Path) -> std::io::Result<()> {
    let r = std::fs::remove_dir(path);
    if r.is_ok() {
        mark_mutated();
    }
    r
}

/// `Path.replace(target)` / `os.replace`.
pub fn replace(src: &Path, dst: &Path) -> Result<()> {
    mark_mutated();
    std::fs::rename(src, dst).map_err(|e| io_err(src, e))
}

/// Append bytes to a file, creating it (`open(path, "a")`).
pub fn append(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    mark_mutated();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| io_err(path, e))?;
    f.write_all(data).map_err(|e| io_err(path, e))
}

/// `hashlib.sha256(path.read_bytes()).hexdigest()`, streamed.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut h = tmi_store::oid::Sha256::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| io_err(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.hexdigest())
}

/// `Path.read_bytes()`.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| io_err(path, e))
}
