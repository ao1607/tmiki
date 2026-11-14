//! The repository operations on remotes: repo.py `Repo.create`, `fetch`,
//! `push`, `pull` (the fast-forward part of `merge`) and cli.py
//! `cmd_clone`, in Python's order of reads, writes and checks.

use crate::{open, Expect, Progress, Remote, Updated};
use std::path::{Path, PathBuf};
use tmi_repo::repo::{prefix_chars, Entries, DEFAULT_BRANCH, TMI_DIR};
use tmi_repo::{fsio, hooks, py, Repo, RepoError, Result};
use tmi_store::json::Value;

const INIT_DIRS: [&str; 8] = ["objects", "refs/heads", "refs/tags", "refs/remotes", "logs", "diff", "conflicts", "hooks"];

/// Forward when making `dir` (and its parents) would meet a file:
/// `mkdir(parents=True, exist_ok=True)`'s FileExistsError text is Python's.
fn check_mkdir(dir: &Path) -> Result<()> {
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.exists() {
            if !p.is_dir() {
                return Err(RepoError::forward("a file where init needs a directory"));
            }
            return Ok(());
        }
        cur = p.parent();
    }
    Ok(())
}

/// `Repo.create(path, bare)`: the repository directory, its subdirectories,
/// HEAD, config and index (each only when missing).
pub fn create(path: &Path, bare: bool) -> Result<Repo> {
    let target = py::realpath(if path.as_os_str().is_empty() { Path::new(".") } else { path });
    let repo_dir = if bare { target.clone() } else { target.join(TMI_DIR) };
    check_mkdir(&repo_dir)?;
    for rel in INIT_DIRS {
        check_mkdir(&tmi_repo::repo::join_trusted(&repo_dir, rel))?;
    }
    fsio::mkdir_p(&repo_dir)?;
    for rel in INIT_DIRS {
        fsio::mkdir_p(&tmi_repo::repo::join_trusted(&repo_dir, rel))?;
    }
    let head = repo_dir.join("HEAD");
    if !head.exists() {
        fsio::atomic_write(&head, format!("ref: refs/heads/{DEFAULT_BRANCH}\n").as_bytes())?;
    }
    let config = repo_dir.join("config");
    if !config.exists() {
        fsio::atomic_write(&config, b"{\"version\":1,\"remotes\":{}}\n")?;
    }
    let repo = Repo::new(if bare { None } else { Some(py::realpath(&target)) }, py::realpath(&repo_dir));
    if !repo.repo_dir.join("index").exists() {
        repo.write_index_entries(Entries::new())?;
    }
    Ok(repo)
}

/// `self.remotes().get(name)["url"]`, with `remote_transport`'s error.
pub fn remote_url(repo: &Repo, name: &str) -> Result<String> {
    let config = repo.read_config()?;
    let remote = match config.get("remotes") {
        None => None,
        Some(Value::Object(m)) => m.get(name),
        Some(_) => return Err(RepoError::forward("remotes is not a dict")),
    };
    let Some(remote) = remote.filter(|r| r.truthy()) else {
        return Err(RepoError::runtime(format!("remote not found: {name}")));
    };
    match remote.get("url") {
        Some(Value::Str(url)) => Ok(url.clone()),
        _ => Err(RepoError::forward("a remote without a url string")),
    }
}

pub struct Fetched {
    pub objects: usize,
    /// branch -> commit ("" for an empty ref), in the remote's order.
    pub refs: Vec<(String, String)>,
    /// Tags that were new here (issue #40).
    pub new_tags: Vec<String>,
}

/// repo.py `take_remote_tags(all_refs)`: make the remote's tags local too; a
/// tag we already have keeps its value. Returns the new ones.
pub fn take_remote_tags(repo: &Repo, refs: &crate::Refs) -> Result<Vec<String>> {
    let mut tags: Vec<(&String, &String)> =
        refs.iter().filter(|(r, o)| r.starts_with("refs/tags/") && !o.is_empty()).collect();
    tags.sort();
    let mut new = Vec::new();
    for (r, oid) in tags {
        if repo.read_ref(r)?.is_none() && repo.store.exists(oid) {
            repo.write_ref(r, oid)?;
            new.push(r.strip_prefix("refs/tags/").unwrap_or(r).to_string());
        }
    }
    Ok(new)
}

fn head_refs(refs: &crate::Refs) -> Vec<(String, String)> {
    refs.iter()
        .filter_map(|(r, v)| r.strip_prefix("refs/heads/").map(|b| (b.to_string(), v.clone())))
        .collect()
}

/// `Repo.fetch(name)`.
pub fn fetch(repo: &Repo, name: &str, progress: Progress) -> Result<Fetched> {
    let remote = open(&remote_url(repo, name)?)?;
    let objects = remote.pull_objects(&repo.store, progress)?;
    let all_refs = remote.list_refs()?;
    let refs = head_refs(&all_refs);
    for (branch, commit) in &refs {
        if !commit.is_empty() {
            repo.write_ref(&format!("refs/remotes/{name}/{branch}"), commit)?;
        }
    }
    let new_tags = take_remote_tags(repo, &all_refs)?;
    Ok(Fetched { objects, refs, new_tags })
}

pub struct Pushed {
    pub objects: usize,
    pub branch: String,
    pub commit: String,
    pub forced: bool,
}

/// `Repo.push(name, branch, force=, force_with_lease=)`. `before_hook`
/// runs right before the pre-push hook starts.
pub fn push(
    repo: &Repo,
    name: &str,
    branch: Option<&str>,
    force: bool,
    force_with_lease: bool,
    progress: Progress,
    before_hook: &mut dyn FnMut(),
) -> Result<Pushed> {
    let branch = match branch.filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => repo.current_branch()?.filter(|b| !b.is_empty()).ok_or_else(|| RepoError::runtime("cannot infer current branch"))?,
    };
    let commit = repo
        .read_ref(&format!("refs/heads/{branch}"))?
        .ok_or_else(|| RepoError::runtime(format!("branch has no commit: {branch}")))?;
    let remote = open(&remote_url(repo, name)?)?;
    let remote_commit = remote.list_refs()?.get(&format!("refs/heads/{branch}")).cloned().unwrap_or_default();
    let tracking = format!("refs/remotes/{name}/{branch}");
    let tracked = repo.read_ref(&tracking)?;
    let mut forced = false;
    if !remote_commit.is_empty() && remote_commit != commit && !repo.is_ancestor(&remote_commit, &commit)? {
        if force_with_lease {
            let Some(tracked) = tracked else {
                return Err(RepoError::runtime(format!(
                    "cannot force-with-lease without a local tracking ref for {name}/{branch}; fetch first"
                )));
            };
            if tracked != remote_commit {
                return Err(RepoError::runtime(format!(
                    "force-with-lease rejected: remote {name}/{branch} is at {}, but local tracking ref is {}. Fetch first.",
                    prefix_chars(&remote_commit, 12),
                    prefix_chars(&tracked, 12)
                )));
            }
            forced = true;
        } else if force {
            forced = true;
        } else {
            return Err(RepoError::runtime(format!(
                "updates were rejected: remote {name}/{branch} is at {}, which is not an ancestor of {} \
                 (non-fast-forward). Fetch and merge first, or pass --force to overwrite.",
                prefix_chars(&remote_commit, 12),
                prefix_chars(&commit, 12)
            )));
        }
    }
    hooks::run(
        &repo.repo_dir,
        "pre-push",
        &[("TMI_REMOTE", name), ("TMI_BRANCH", &branch), ("TMI_COMMIT", &commit), ("TMI_REMOTE_COMMIT", &remote_commit)],
        true,
        before_hook,
    )?;
    let objects = remote.push_objects(&repo.store, &repo.closure(std::slice::from_ref(&commit))?, progress)?;
    // Compare-and-swap against the remote tip this push checked (the remote
    // ref is updated under its lock): a push that raced another one is
    // rejected like a non-fast-forward instead of overwriting it. A plain
    // --force is the one update that does not compare.
    let expect = if forced && !force_with_lease { Expect::Any } else { Expect::Is(Some(remote_commit.as_str()).filter(|c| !c.is_empty())) };
    if let Updated::Moved(actual) = remote.update_ref(&format!("refs/heads/{branch}"), &commit, expect)? {
        return Err(RepoError::runtime(format!(
            "updates were rejected: remote {name}/{branch} is at {}, which is not an ancestor of {} \
             (non-fast-forward). Fetch and merge first, or pass --force to overwrite.",
            actual.as_deref().map_or_else(|| "nothing".to_string(), |a| prefix_chars(a, 12)),
            prefix_chars(&commit, 12)
        )));
    }
    repo.write_ref(&tracking, &commit)?;
    Ok(Pushed { objects, branch, commit, forced })
}

pub struct PushedTags {
    pub objects: usize,
    /// (tag, value) of the tags sent.
    pub pushed: Vec<(String, String)>,
}

/// repo.py `Repo.is_tag_push(ref)`: whether `push <remote> <ref>` pushes a tag.
pub fn is_tag_push(repo: &Repo, r: Option<&str>) -> Result<bool> {
    let Some(r) = r.filter(|r| !r.is_empty()) else { return Ok(false) };
    if r.starts_with("refs/tags/") {
        return Ok(true);
    }
    Ok(repo.read_ref(&format!("refs/heads/{r}"))?.is_none() && repo.read_ref(&format!("refs/tags/{r}"))?.is_some())
}

/// repo.py `Repo.push_tags(name, tags, force=)`: `tags` None is every tag.
pub fn push_tags(
    repo: &Repo,
    name: &str,
    tags: Option<&[String]>,
    force: bool,
    progress: Progress,
    before_hook: &mut dyn FnMut(),
) -> Result<PushedTags> {
    let local: std::collections::BTreeMap<String, String> = repo.list_tags()?.into_iter().collect();
    let wanted: Vec<String> = match tags {
        None => local.keys().cloned().collect(),
        Some(t) => t.iter().map(|t| t.strip_prefix("refs/tags/").unwrap_or(t).to_string()).collect(),
    };
    for tag in &wanted {
        if !local.contains_key(tag) {
            return Err(RepoError::runtime(format!("tag not found: {tag}")));
        }
    }
    let remote = open(&remote_url(repo, name)?)?;
    let remote_refs = remote.list_refs()?;
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for tag in &wanted {
        let value = &local[tag];
        let remote_value = remote_refs.get(&format!("refs/tags/{tag}")).cloned().unwrap_or_default();
        if remote_value == *value {
            continue;
        }
        if !remote_value.is_empty() && !force {
            return Err(RepoError::runtime(format!(
                "updates were rejected: tag {tag} already exists on {name} at {}, not {}. Tags are not moved; pass --force to replace it.",
                prefix_chars(&remote_value, 12),
                prefix_chars(value, 12)
            )));
        }
        pending.push((tag.clone(), value.clone(), remote_value));
    }
    for (tag, value, remote_value) in &pending {
        hooks::run(
            &repo.repo_dir,
            "pre-push",
            &[("TMI_REMOTE", name), ("TMI_BRANCH", ""), ("TMI_TAG", tag), ("TMI_COMMIT", value), ("TMI_REMOTE_COMMIT", remote_value)],
            true,
            before_hook,
        )?;
    }
    let objects = if pending.is_empty() {
        0
    } else {
        let roots: Vec<String> = pending.iter().map(|(_, value, _)| value.clone()).collect();
        remote.push_objects(&repo.store, &repo.closure(&roots)?, progress)?
    };
    for (tag, value, remote_value) in &pending {
        // Compare-and-swap against the tag value checked above (issue #36);
        // --force is the one update that does not compare.
        let expect = if force { Expect::Any } else { Expect::Is(Some(remote_value.as_str()).filter(|c| !c.is_empty())) };
        if let Updated::Moved(actual) = remote.update_ref(&format!("refs/tags/{tag}"), value, expect)? {
            return Err(RepoError::runtime(format!(
                "updates were rejected: tag {tag} was changed on {name} while pushing (now {}, was {}). \
                 Fetch first; tags are not moved, pass --force to replace it.",
                actual.as_deref().map_or_else(|| "missing".to_string(), |a| prefix_chars(a, 12)),
                if remote_value.is_empty() { "missing".to_string() } else { prefix_chars(remote_value, 12) }
            )));
        }
    }
    Ok(PushedTags { objects, pushed: pending.into_iter().map(|(t, v, _)| (t, v)).collect() })
}

pub enum Pulled {
    Initial(String),
    UpToDate(String),
    FastForward(String),
    /// `merge(incoming, message, operation=operation)` still to run.
    Merge { incoming: String, operation: String, message: String },
}

impl Pulled {
    pub fn mode(&self) -> &'static str {
        match self {
            Pulled::Initial(_) => "initial",
            Pulled::UpToDate(_) => "already-up-to-date",
            Pulled::FastForward(_) => "fast-forward",
            Pulled::Merge { .. } => "merge",
        }
    }

    pub fn commit(&self) -> &str {
        match self {
            Pulled::Initial(c) | Pulled::UpToDate(c) | Pulled::FastForward(c) => c,
            Pulled::Merge { .. } => "",
        }
    }
}

/// `Repo.pull(name, branch)`. A merge that is not a fast-forward comes back
/// as `Pulled::Merge` for the caller to run. By then this run has fetched
/// (`fetched` is set), having written only objects (content-addressed) and
/// remote-tracking refs, so the mark is cleared: a Forward from here on
/// lets Python continue from the merge (`TMI_PULL_FETCHED`, cli.py
/// `cmd_pull`).
pub fn pull(
    repo: &Repo,
    name: &str,
    branch: Option<&str>,
    progress: Progress,
    warnings: &mut Vec<String>,
    fetched: &mut bool,
    before_hook: &mut dyn FnMut(),
) -> Result<Pulled> {
    fetch(repo, name, progress)?;
    *fetched = true;
    fsio::unmark_mutated();
    let branch = match branch.filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => repo.current_branch()?.filter(|b| !b.is_empty()).unwrap_or_else(|| DEFAULT_BRANCH.to_string()),
    };
    let incoming = repo
        .read_ref(&format!("refs/remotes/{name}/{branch}"))?
        .ok_or_else(|| RepoError::runtime(format!("remote branch not found: {name}/{branch}")))?;
    let operation = format!("pull {name}/{branch}");
    let Some(current) = repo.current_commit()? else {
        repo.ensure_clean_tracked("pull")?;
        repo.ensure_no_untracked_overwrite(&repo.entries_for_checkout(&incoming)?, None, "pull")?;
        repo.set_current_commit(&incoming)?;
        repo.restore_all(&incoming, None, &operation)?;
        let reason = format!("pull: {name}/{branch}");
        repo.log_ref_move("HEAD", None, &incoming, &reason)?;
        let target = repo.head_target()?;
        if target.starts_with("refs/heads/") {
            repo.log_ref_move(&target, None, &incoming, &reason)?;
        }
        return Ok(Pulled::Initial(incoming));
    };
    // merge(incoming, f"Merge {name}/{branch}", operation=f"pull {name}/{branch}")
    if repo.pending_merge_head()?.is_some() {
        return Err(RepoError::runtime(
            "merge already in progress; resolve it with 'tmiki add' and 'tmiki commit' or use 'tmiki switch --discard'",
        ));
    }
    let other = repo
        .resolve_rev(Some(&incoming), warnings)?
        .ok_or_else(|| RepoError::runtime(format!("unknown merge target: {incoming}")))?;
    if current == other || repo.is_ancestor(&other, &current)? {
        return Ok(Pulled::UpToDate(current));
    }
    if !repo.is_ancestor(&current, &other)? {
        // a true merge: the caller runs it (tmi-diffmerge, stage 4a)
        return Ok(Pulled::Merge { incoming, operation, message: format!("Merge {name}/{branch}") });
    }
    repo.ensure_clean_tracked("merge")?;
    repo.ensure_no_untracked_overwrite(&repo.entries_for_checkout(&other)?, Some(&current), "merge")?;
    repo.set_current_commit(&other)?;
    repo.restore_all(&other, Some(&current), &operation)?;
    let reason = format!("merge ff: {incoming}");
    repo.log_ref_move("HEAD", Some(&current), &other, &reason)?;
    let target = repo.head_target()?;
    if target.starts_with("refs/heads/") {
        repo.log_ref_move(&target, Some(&current), &other, &reason)?;
    }
    hooks::run(
        &repo.repo_dir,
        "post-merge",
        &[("TMI_COMMIT", &other), ("TMI_MERGED_FROM", &other)],
        false,
        before_hook,
    )?;
    Ok(Pulled::FastForward(other))
}

/// `PureWindowsPath(s).name` / `PurePosixPath(s).name`.
pub fn path_name(s: &str) -> String {
    let mut rest = s;
    if cfg!(windows) {
        let b = s.as_bytes();
        if b.len() >= 2 && b[1] == b':' && s.is_char_boundary(2) {
            rest = &s[2..];
        } else if (s.starts_with("\\\\") || s.starts_with("//")) && s.len() > 2 {
            // UNC: \\server\share is the drive
            let body = &s[2..];
            let mut parts = body.splitn(3, ['\\', '/']);
            let _server = parts.next();
            let _share = parts.next();
            rest = parts.next().unwrap_or("");
        }
    }
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    rest.split(seps).filter(|p| !p.is_empty() && *p != ".").last().unwrap_or("").to_string()
}

pub struct Cloned {
    pub stored_url: String,
    pub target: PathBuf,
    pub objects: usize,
}

/// cli.py `cmd_clone(url, directory)`. `after_objects` runs when the
/// objects are in (cli.py's `progress.done()`).
pub fn clone(url: &str, directory: Option<&str>, progress: Progress, after_objects: &mut dyn FnMut()) -> Result<Cloned> {
    let remote = open(url)?;
    let target = match directory {
        Some(d) => py::realpath(if d.is_empty() { Path::new(".") } else { Path::new(d) }),
        None => {
            let mut leaf = path_name(url);
            if leaf.is_empty() {
                leaf = "repo".into();
            }
            for ext in [".tmi", ".git"] {
                if leaf.ends_with(ext) && leaf.len() > ext.len() {
                    leaf.truncate(leaf.len() - ext.len());
                    break;
                }
            }
            let cwd = std::env::current_dir().map_err(|e| RepoError::forward(e.to_string()))?;
            py::realpath(&cwd.join(leaf))
        }
    };
    if target.exists() {
        if !target.is_dir() {
            return Err(RepoError::runtime(format!("target exists and is not a directory: {}", target.display())));
        }
        let mut entries = std::fs::read_dir(&target).map_err(|e| fsio::io_err(&target, e))?;
        if entries.next().is_some() {
            return Err(RepoError::runtime(format!("target directory is not empty: {}", target.display())));
        }
    }
    // Decided before the first write: the URL as add_remote stores it, and
    // for a local remote (read-only so far) the branch to check out.
    let canonical = tmi_repo::refs_write::canonical_url(url)?;
    if let Remote::Local(local) = &remote {
        let refs = head_refs(&local.list_refs()?);
        if local.head_branch()?.is_none() && refs.len() > 1 && !refs.iter().any(|(b, _)| b == DEFAULT_BRANCH) {
            return Err(RepoError::forward("a detached remote HEAD: Python picks the branch in rglob's order"));
        }
    }
    check_mkdir(&target)?;
    fsio::mkdir_p(&target)?;
    let repo = create(&target, false)?;
    repo.add_remote("origin", url)?;
    let pulled = remote.pull_objects(&repo.store, progress);
    after_objects();
    let objects = pulled?;
    let all_refs = remote.list_refs()?;
    let refs = head_refs(&all_refs);
    for (branch, commit) in &refs {
        if !commit.is_empty() {
            repo.write_ref(&format!("refs/remotes/origin/{branch}"), commit)?;
        }
    }
    take_remote_tags(&repo, &all_refs)?;
    let default_branch: Option<String> = match remote.head_branch()? {
        Some(value) => value,
        None => Some(if refs.iter().any(|(b, _)| b == DEFAULT_BRANCH) {
            DEFAULT_BRANCH.to_string()
        } else {
            refs.first().map_or(DEFAULT_BRANCH.to_string(), |(b, _)| b.clone())
        }),
    };
    if let Some(branch) = default_branch {
        if let Some((_, commit)) = refs.iter().find(|(b, _)| *b == branch).filter(|(_, c)| !c.is_empty()) {
            repo.write_ref(&format!("refs/heads/{branch}"), commit)?;
            repo.set_head_ref(&format!("refs/heads/{branch}"))?;
            repo.restore_all(commit, None, "clone")?;
        }
    }
    Ok(Cloned { stored_url: canonical, target, objects })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(path_name("http://127.0.0.1:5000/api/u/repo"), "repo");
        assert_eq!(path_name("../x/hub.tmi/"), "hub.tmi");
        assert_eq!(path_name("."), "");
        assert_eq!(path_name("a/.."), "..");
    }
}
