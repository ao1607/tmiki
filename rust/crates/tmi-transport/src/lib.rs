//! tmi/transport in Rust (RUST_MIGRATION.md 4b): remotes on a local path
//! (`LocalTransport`) and tmihub over HTTP(S) (`HttpsTransport`), and the
//! repository operations on top of them (`Repo.fetch`, `push`, `pull`,
//! cli.py's `cmd_clone`, `Repo.create`).
//!
//! Objects moved, refs written, progress messages and error texts are
//! Python's. What is not reproduced returns `RepoError::Forward` before
//! anything is written: a proxy urllib would use, a first request that
//! could not connect (Python then reports it in its words; nothing goes to
//! Python once a request has reached the server), `file://` and `~` URLs
//! for clone, and a true merge in `pull` (after the fetch, whose writes
//! Python would repeat identically; see `ops::pull`).

pub mod https;
pub mod local;
pub mod ops;
pub mod proxy;
pub mod rawobj;
pub mod zipio;

use indexmap::IndexMap;
pub use tmi_repo::lockfile::Expect;
use tmi_repo::{RepoError, Result};
use tmi_store::ObjectStore;

/// The result of a remote ref update.
#[derive(Debug, PartialEq, Eq)]
pub enum Updated {
    Done,
    /// A compare-and-swap found the ref at this value (None: missing)
    /// instead of the expected one; nothing was changed.
    Moved(Option<String>),
}

/// `on_progress(msg)`: called from worker threads too.
pub type Progress<'a> = &'a (dyn Fn(&str) + Sync);

/// The remote's refs as `list_refs()` returns them, in its order.
pub type Refs = IndexMap<String, String>;

/// `urlsplit(url).scheme` (lowercased; "" when there is none). URLs
/// urlsplit would clean first (leading controls or spaces, tabs and
/// newlines) are Forward.
pub fn url_scheme(url: &str) -> Result<String> {
    if url.starts_with(|c: char| c <= ' ') || url.contains(['\t', '\r', '\n']) {
        return Err(RepoError::forward("URL urlsplit would clean"));
    }
    Ok(match url.find(':') {
        Some(i) if i > 0 => {
            let cand = &url[..i];
            let first = cand.chars().next().unwrap();
            if first.is_ascii_alphabetic() && cand.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
                cand.to_ascii_lowercase()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    })
}

/// transport `_is_local_url(url)`: no scheme, `file`, or a drive letter.
pub fn is_local_url(url: &str) -> Result<bool> {
    let scheme = url_scheme(url)?;
    Ok(scheme.is_empty() || scheme == "file" || scheme.chars().count() == 1)
}

pub enum Remote {
    Local(local::LocalRemote),
    Https(https::HttpsRemote),
}

/// transport `open_transport(url)`.
pub fn open(url: &str) -> Result<Remote> {
    if is_local_url(url)? {
        return Ok(Remote::Local(local::LocalRemote::open(url)?));
    }
    let scheme = url_scheme(url)?;
    if scheme == "http" || scheme == "https" {
        return Ok(Remote::Https(https::HttpsRemote::new(url)?));
    }
    Err(RepoError::runtime(format!("unsupported remote URL scheme: {scheme}://")))
}

/// transport `checked_refs(refs)`: what a remote lists is data other people
/// write, so every branch and tag name must be one tmi could write
/// (`ref_name_problem`) and every value empty or a 64-digit lower-case hex
/// object id. The whole listing is refused otherwise, before any ref is
/// written (an id reaches ref files, messages and the pre-push hook's
/// environment).
pub fn check_refs(refs: Refs) -> Result<Refs> {
    for (name, value) in &refs {
        if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
            if let Some(problem) = tmi_repo::repo::ref_name_problem(name, false) {
                return Err(RepoError::runtime(format!(
                    "remote sent an invalid ref name: {} ({problem})",
                    tmi_repo::textsafe::escape(name, false)
                )));
            }
        }
        let ok = value.is_empty() || (value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        if !ok {
            return Err(RepoError::runtime(format!(
                "remote sent an invalid object id for {}: {}",
                tmi_repo::textsafe::escape(name, false),
                tmi_repo::textsafe::escape(value, false)
            )));
        }
    }
    Ok(refs)
}

impl Remote {
    /// The remote's refs, checked by `check_refs`.
    pub fn list_refs(&self) -> Result<Refs> {
        check_refs(match self {
            Remote::Local(r) => r.list_refs()?,
            Remote::Https(r) => r.list_refs()?,
        })
    }

    pub fn pull_objects(&self, into: &ObjectStore, progress: Progress) -> Result<usize> {
        match self {
            Remote::Local(r) => r.pull_objects(into, progress),
            Remote::Https(r) => r.pull_objects(into, progress),
        }
    }

    pub fn push_objects(&self, from: &ObjectStore, progress: Progress) -> Result<usize> {
        match self {
            Remote::Local(r) => r.push_objects(from, progress),
            Remote::Https(r) => r.push_objects(from, progress),
        }
    }

    /// Point the remote ref at `value`; with `Expect::Is` a compare-and-swap:
    /// `Updated::Moved(actual)` (nothing changed) when the remote ref is no
    /// longer the expected one (issue #36).
    pub fn update_ref(&self, name: &str, value: &str, expect: Expect<'_>) -> Result<Updated> {
        match self {
            Remote::Local(r) => r.update_ref(name, value, expect),
            Remote::Https(r) => r.update_ref(name, value, expect).map(|()| Updated::Done),
        }
    }

    /// `head_branch()`: None, or the value (`Some(None)` stands for a value
    /// that is not a string, which names no branch).
    pub fn head_branch(&self) -> Result<Option<Option<String>>> {
        match self {
            Remote::Local(r) => Ok(r.head_branch()?.map(Some)),
            Remote::Https(r) => r.head_branch(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Remote::Local(_))
    }
}

/// `int(os.environ.get(name, ""))` for plain decimal text (a sign,
/// surrounding whitespace, `_` between digits); None where `int()` raises.
pub fn py_int(text: &str) -> Option<i128> {
    let t = text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let mut value: i128 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(10)?;
        value = value.checked_mul(10)?.checked_add(d as i128)?;
    }
    Some(if neg { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        assert_eq!(url_scheme("https://h/api/u/r").unwrap(), "https");
        assert_eq!(url_scheme(r"C:\x\y").unwrap(), "c");
        assert_eq!(url_scheme("../r").unwrap(), "");
        assert!(is_local_url("file:///C:/x").unwrap());
        assert!(!is_local_url("ssh://x").unwrap());
    }

    #[test]
    fn ints() {
        assert_eq!(py_int(" 12 "), Some(12));
        assert_eq!(py_int("1_000"), Some(1000));
        assert_eq!(py_int("-3"), Some(-3));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int(""), None);
        assert_eq!(py_int("x"), None);
    }
}
