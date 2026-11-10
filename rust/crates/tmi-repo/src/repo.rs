//! tmi/repo.py `Repo`, the read side: discovery, HEAD and refs, revision
//! resolution, commits and trees, the index, merge state.

use crate::error::{RepoError, Result};
use crate::py;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tmi_store::json::{self, Value};
use tmi_store::ObjectStore;

pub const TMI_DIR: &str = ".tmi";
pub const DEFAULT_BRANCH: &str = "main";

pub type Entries = IndexMap<String, Value>;

pub struct Repo {
    pub worktree: Option<PathBuf>,
    pub repo_dir: PathBuf,
    pub store: ObjectStore,
    /// Working-tree files this run rewrote, in order (repo.py `rewritten`).
    pub rewritten: std::sync::Mutex<Vec<PathBuf>>,
    /// `_safe_identity()`, read once (see `write::prepare_identity`).
    pub(crate) identity: std::sync::OnceLock<Option<(String, String)>>,
}

pub use crate::recpath::{join_rel, join_trusted, RelPath};

/// `str.strip()` (Python's whitespace includes U+001C..U+001F).
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// `Path.read_text(encoding="utf-8")`: None when the file does not exist.
pub fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| RepoError::forward(format!("{} is not UTF-8", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RepoError::forward(format!("{}: {e}", path.display()))),
    }
}

/// `repr(s)` for the plain printable-ASCII texts the CLI quotes; Forward
/// otherwise (Python's escaping of other characters is not reproduced).
pub fn py_repr_ascii(s: &str) -> Result<String> {
    if !s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Ok(crate::py::repr_lenient(s));
    }
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        if c == '\\' || c == quote {
            out.push('\\');
        }
        out.push(c);
    }
    out.push(quote);
    Ok(out)
}

/// repo.py `ref_name_problem(name)`: why `name` (a full ref, or a branch /
/// tag / remote name) cannot name a ref (`git check-ref-format`'s rules and
/// the characters Windows file names cannot hold), or None. Same order and
/// texts as Python (issue #41).
///
/// `reading`: only what could never be a file under refs/ (what tmi has
/// always refused): a revision lookup tries `HEAD~1` as a ref name first.
pub fn ref_name_problem(name: &str, reading: bool) -> Option<String> {
    if name.is_empty() {
        return Some("it is empty".into());
    }
    for ch in name.chars() {
        if (ch as u32) < 32 {
            return Some("it contains a control character".into());
        }
        if "\\:?*<>\"|".contains(ch) {
            return Some(format!("it contains '{ch}'"));
        }
        if reading {
            continue;
        }
        if ch as u32 == 127 || (0x80..=0x9f).contains(&(ch as u32)) || is_format_char(ch) {
            return Some("it contains a control character".into());
        }
        if ch == ' ' {
            return Some("it contains a space".into());
        }
        if "~^[".contains(ch) {
            return Some(format!("it contains '{ch}'"));
        }
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Some("it has an empty part (a leading, trailing or doubled '/')".into());
    }
    if reading {
        if parts.iter().any(|p| *p == "." || *p == "..") {
            return Some("a part of it is '.' or '..'".into());
        }
        return None;
    }
    if name.contains("..") {
        return Some("it contains '..'".into());
    }
    if parts.iter().any(|p| p.starts_with('.')) {
        return Some("a part of it starts with '.'".into());
    }
    if name.contains("@{") {
        return Some("it contains '@{'".into());
    }
    if parts.iter().any(|p| p.ends_with(".lock")) {
        return Some("a part of it ends with '.lock'".into());
    }
    if name.ends_with('.') {
        return Some("it ends with '.'".into());
    }
    if name == "@" {
        return Some("it is '@'".into());
    }
    if parts.iter().any(|p| crate::recpath::is_reserved_device(p)) {
        return Some("a part of it is a reserved Windows device name".into());
    }
    None
}

/// Bidi controls and marks (U+200E / U+200F, U+202A-U+202E, U+2066-U+2069): a
/// name that reads backwards in a terminal. Same set as repo.py `_FORMAT_CHARS`.
pub fn is_format_char(ch: char) -> bool {
    matches!(ch as u32, 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
}

/// repo.py `check_ref_short_name(name)`: a new branch, tag or remote name.
pub fn check_ref_short_name(name: &str) -> Result<()> {
    let problem = ref_name_problem(name, false).or_else(|| {
        if name == "HEAD" {
            Some("HEAD is reserved".to_string())
        } else if name.starts_with('-') {
            Some("it starts with '-'".to_string())
        } else {
            None
        }
    });
    match problem {
        Some(p) => Err(RepoError::runtime(format!("invalid ref name: {name} ({p})"))),
        None => Ok(()),
    }
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Repo {
    pub fn new(worktree: Option<PathBuf>, repo_dir: PathBuf) -> Repo {
        let store = ObjectStore::new(repo_dir.join("objects"));
        Repo { worktree, repo_dir, store, rewritten: Default::default(), identity: Default::default() }
    }

    /// `Repo.open()`: the nearest directory, from the cwd up, holding `.tmi`.
    pub fn open() -> Result<Repo> {
        let cwd = std::env::current_dir().map_err(|e| RepoError::forward(e.to_string()))?;
        Repo::open_at(&cwd)
    }

    pub fn open_at(start: &Path) -> Result<Repo> {
        let start = py::realpath(start);
        let mut cur = if start.is_dir() {
            start.clone()
        } else {
            start.parent().map(Path::to_path_buf).unwrap_or(start.clone())
        };
        loop {
            let repo_dir = cur.join(TMI_DIR);
            if repo_dir.is_dir() {
                return Ok(Repo::new(Some(cur.clone()), repo_dir));
            }
            match cur.parent() {
                Some(parent) if parent != cur => cur = parent.to_path_buf(),
                _ => return Err(RepoError::runtime("not a tmi repository")),
            }
        }
    }

    pub fn require_worktree(&self) -> Result<&Path> {
        self.worktree.as_deref().ok_or_else(|| RepoError::runtime("operation requires a working tree"))
    }

    pub fn head_target(&self) -> Result<String> {
        let path = self.repo_dir.join("HEAD");
        let text = read_text(&path)?.ok_or_else(|| RepoError::forward("HEAD is missing"))?;
        let head = py_strip(&text);
        Ok(head.strip_prefix("ref: ").unwrap_or(head).to_string())
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.head_target()?.strip_prefix("refs/heads/").map(str::to_string))
    }

    pub fn current_commit(&self) -> Result<Option<String>> {
        let target = self.head_target()?;
        if target.starts_with("refs/") {
            return self.read_ref(&target);
        }
        Ok((!target.is_empty()).then_some(target))
    }

    /// `_ref_path(ref)`: the file of a ref name, after Python's checks (Git's
    /// whole rule for ref names: writes).
    pub fn ref_path(&self, name: &str) -> Result<PathBuf> {
        self.ref_path_for(name, false)
    }

    /// `_ref_path(ref, strict=False)`: reads and deletes accept what could
    /// exist as a file (a revision lookup tries `HEAD~1` as a name).
    pub fn ref_path_lenient(&self, name: &str) -> Result<PathBuf> {
        self.ref_path_for(name, true)
    }

    fn ref_path_for(&self, name: &str, reading: bool) -> Result<PathBuf> {
        let problem = ref_name_problem(name, reading).or_else(|| {
            (name.split('/').next() != Some("refs")).then(|| "it does not start with refs/".to_string())
        });
        if let Some(problem) = problem {
            // a branch or tag is shown by its short name
            let shown = if name.starts_with("refs/heads/") {
                name.strip_prefix("refs/heads/").unwrap()
            } else {
                name.strip_prefix("refs/tags/").unwrap_or(name)
            };
            return Err(RepoError::runtime(format!("invalid ref name: {shown} ({problem})")));
        }
        let parts: Vec<&str> = name.split('/').collect();
        let mut path = self.repo_dir.clone();
        for p in parts {
            path.push(p);
        }
        Ok(path)
    }

    pub fn read_ref(&self, name: &str) -> Result<Option<String>> {
        let path = self.ref_path_lenient(name)?;
        if !path.is_file() {
            return Ok(None);
        }
        let text = read_text(&path)?.unwrap_or_default();
        let value = py_strip(&text);
        Ok((!value.is_empty()).then(|| value.to_string()))
    }

    pub fn pending_merge_head(&self) -> Result<Option<String>> {
        Ok(read_text(&self.repo_dir.join("MERGE_HEAD"))?
            .map(|t| py_strip(&t).to_string())
            .filter(|t| !t.is_empty()))
    }

    /// Whether `.tmi/conflicts/<dir>/conflict.json` exists for any dir
    /// (`pending_conflict_paths()` would read them).
    pub fn has_conflict_records(&self) -> bool {
        let Ok(dirs) = std::fs::read_dir(self.repo_dir.join("conflicts")) else {
            return false;
        };
        dirs.flatten().any(|d| d.path().join("conflict.json").exists())
    }

    pub fn read_commit(&self, oid: &str) -> Result<Value> {
        Ok(self.store.read_json(oid, Some("commit"))?)
    }

    /// `resolve_rev(rev)`. Ambiguity warnings Python prints to stderr are
    /// pushed onto `warnings`.
    pub fn resolve_rev(&self, rev: Option<&str>, warnings: &mut Vec<String>) -> Result<Option<String>> {
        let rev = match rev {
            None | Some("") | Some("HEAD") => return self.current_commit(),
            Some(r) => r,
        };
        if rev == "MERGE_HEAD" {
            return match self.pending_merge_head()? {
                Some(head) => Ok(Some(head)),
                None => Err(RepoError::runtime("MERGE_HEAD: no merge in progress")),
            };
        }
        let candidates = [
            format!("refs/tags/{rev}"),
            format!("refs/heads/{rev}"),
            format!("refs/remotes/{rev}"),
            format!("refs/remotes/origin/{rev}"),
        ];
        let mut found = Vec::new();
        for name in &candidates {
            if let Some(value) = self.read_ref(name)? {
                found.push((name.clone(), value));
            }
        }
        if let Some((first_ref, first_value)) = found.first() {
            let resolved = self.resolve_object_as_commit(first_value, rev)?;
            if found.len() > 1 {
                let mut others = HashSet::new();
                for (_, value) in &found[1..] {
                    match self.resolve_object_as_commit(value, rev) {
                        Ok(c) => {
                            others.insert(c);
                        }
                        Err(RepoError::Runtime(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
                others.remove(&resolved);
                if !others.is_empty() {
                    let names: Vec<&str> = found.iter().map(|(r, _)| r.as_str()).collect();
                    warnings.push(format!(
                        "tmiki: warning: refname {} is ambiguous ({}); using {first_ref}",
                        py_repr_ascii(rev)?,
                        names.join(", ")
                    ));
                }
            }
            return Ok(Some(resolved));
        }
        // r"(.+)~(\d+)" fullmatch: '.' stops at "\n"; \d is any Unicode digit
        if let Some(pos) = rev.rfind('~') {
            let (base, steps) = (&rev[..pos], &rev[pos + 1..]);
            if !base.is_empty() && !steps.is_empty() && !rev.contains('\n') {
                if !steps.is_ascii() && steps.chars().all(|c| c.is_numeric()) {
                    return Err(RepoError::forward("non-ASCII digits after ~"));
                }
                if steps.bytes().all(|b| b.is_ascii_digit()) {
                    let steps: u64 = steps.parse().map_err(|_| RepoError::forward("huge ~N"))?;
                    let mut cur = self.resolve_rev(Some(base), warnings)?;
                    for _ in 0..steps {
                        let Some(c) = cur else {
                            return Err(RepoError::runtime(format!("revision has no parent: {rev}")));
                        };
                        let commit = self.read_commit(&c)?;
                        let parents = parents_of(&commit)?;
                        let Some(first) = parents.first() else {
                            return Err(RepoError::runtime(format!("revision has no parent: {rev}")));
                        };
                        cur = Some(first.clone());
                    }
                    return Ok(cur);
                }
            }
        }
        if rev.len() == 64 && is_hex(rev) {
            return Ok(Some(self.resolve_object_as_commit(&rev.to_lowercase(), rev)?));
        }
        if (7..=63).contains(&rev.len()) && is_hex(rev) {
            let prefix = rev.to_lowercase();
            let matches: Vec<String> =
                self.store.iter_object_ids().into_iter().filter(|o| o.starts_with(&prefix)).collect();
            if matches.len() == 1 {
                return Ok(Some(self.resolve_object_as_commit(&matches[0], rev)?));
            }
            if matches.len() > 1 {
                return Err(RepoError::runtime(format!("ambiguous revision prefix: {rev}")));
            }
        }
        Err(RepoError::runtime(format!("unknown revision: {rev}")))
    }

    /// `_resolve_object_as_commit(oid, rev)`: follows tag objects.
    pub fn resolve_object_as_commit(&self, oid: &str, rev: &str) -> Result<String> {
        let mut seen = HashSet::new();
        let mut oid = oid.to_lowercase();
        loop {
            if !seen.insert(oid.clone()) {
                return Err(RepoError::runtime(format!("tag cycle while resolving revision: {rev}")));
            }
            let (object_type, payload) = match self.store.read(&oid) {
                Ok(v) => v,
                Err(tmi_store::StoreError::Missing(_)) => {
                    return Err(RepoError::runtime(format!("unknown revision: {rev}")))
                }
                Err(e) => return Err(e.into()),
            };
            match object_type.as_str() {
                "tag" => {
                    let tag = json::loads_bytes(&payload).map_err(|e| RepoError::forward(e.to_string()))?;
                    match tag.get("object") {
                        Some(Value::Str(target)) if !target.is_empty() => oid = target.to_lowercase(),
                        _ => return Err(RepoError::runtime(format!("tag does not point to a commit: {rev}"))),
                    }
                }
                "commit" => return Ok(oid),
                _ => return Err(RepoError::runtime(format!("revision does not name a commit: {rev}"))),
            }
        }
    }

    /// cli.py `_object_as_commit`: None when it does not resolve (RuntimeError).
    pub fn object_as_commit(&self, oid: Option<&str>) -> Result<Option<String>> {
        let Some(oid) = oid.filter(|o| !o.is_empty()) else {
            return Ok(None);
        };
        match self.resolve_object_as_commit(oid, oid) {
            Ok(c) => Ok(Some(c)),
            Err(RepoError::Runtime(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn entries_for_commit(&self, commit: Option<&str>) -> Result<Entries> {
        let Some(commit) = commit.filter(|c| !c.is_empty()) else {
            return Ok(Entries::new());
        };
        let obj = self.read_commit(commit)?;
        let tree_ref = obj
            .get("tree_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| RepoError::forward("commit without a tree_ref string"))?;
        let tree = self.store.read_json(tree_ref, Some("tree"))?;
        if tree.get("kind").and_then(Value::as_str) != Some("repository") {
            return Err(RepoError::Value(format!("commit tree is not a repository tree: {tree_ref}")));
        }
        let mut out = Entries::new();
        if let Some(entries) = tree.get("entries") {
            let entries = entries.as_array().ok_or_else(|| RepoError::forward("tree entries not a list"))?;
            for entry in entries {
                let path = entry
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RepoError::forward("tree entry without a path string"))?;
                out.insert(path.to_string(), entry.clone());
            }
        }
        Ok(out)
    }

    /// Every file under `.tmi/refs` (posix names relative to `.tmi`) with
    /// its stripped content, plus MERGE_HEAD; sorted by name.
    pub fn all_refs_sorted(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let base = self.repo_dir.join("refs");
        if base.is_dir() {
            for (rel, path) in walk_files(&base)? {
                let text = read_text(&path)?.unwrap_or_default();
                let value = py_strip(&text);
                if !value.is_empty() {
                    out.push((format!("refs/{rel}"), value.to_string()));
                }
            }
        }
        let head = self.repo_dir.join("MERGE_HEAD");
        if head.exists() {
            if let Some(text) = read_text(&head)? {
                let value = py_strip(&text);
                if !value.is_empty() {
                    out.push(("MERGE_HEAD".into(), value.to_string()));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// `list_head_refs()` (names relative to refs/heads, value or None).
    pub fn list_head_refs(&self) -> Result<Vec<(String, Option<String>)>> {
        let root = self.repo_dir.join("refs").join("heads");
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        for (rel, path) in walk_files(&root)? {
            let text = read_text(&path)?.unwrap_or_default();
            let value = py_strip(&text);
            out.push((rel, (!value.is_empty()).then(|| value.to_string())));
        }
        Ok(out)
    }

    /// `list_tags()`: (name, stripped content), sorted.
    pub fn list_tags(&self) -> Result<Vec<(String, String)>> {
        let root = self.repo_dir.join("refs").join("tags");
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        for (rel, path) in walk_files(&root)? {
            let text = read_text(&path)?.unwrap_or_default();
            out.push((rel, py_strip(&text).to_string()));
        }
        out.sort();
        Ok(out)
    }

    /// `ancestor_set(commit)`: unreadable commits are skipped.
    pub fn ancestor_set(&self, commit: &str) -> Result<HashSet<String>> {
        let mut seen = HashSet::new();
        let mut stack = vec![commit.to_string()];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            match self.read_commit(&cur) {
                Ok(obj) => {
                    if let Some(parents) = obj.get("parents") {
                        if parents.truthy() {
                            let list = parents.as_array().ok_or_else(|| RepoError::forward("parents not a list"))?;
                            for p in list {
                                stack.push(p.as_str().ok_or_else(|| RepoError::forward("parent not a str"))?.into());
                            }
                        }
                    }
                }
                Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
                Err(_) => continue,
            }
        }
        Ok(seen)
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        Ok(self.ancestor_set(descendant)?.contains(ancestor))
    }

    /// `read_index()`.
    pub fn read_index(&self) -> Result<Value> {
        match read_text(&self.repo_dir.join("index"))? {
            None => Ok(json::loads(r#"{"version":1,"entries":{}}"#).unwrap()),
            Some(text) => json::loads(&text).map_err(|e| RepoError::forward(format!("index: {e}"))),
        }
    }

    /// `read_config()` (the repo config).
    pub fn read_config(&self) -> Result<Value> {
        match read_text(&self.repo_dir.join("config"))? {
            None => Ok(json::loads(r#"{"version":1,"remotes":{}}"#).unwrap()),
            Some(text) => json::loads(&text).map_err(|e| RepoError::forward(format!("config: {e}"))),
        }
    }
}

/// `commit.get("parents") or []` as strings.
pub fn parents_of(commit: &Value) -> Result<Vec<String>> {
    match commit.get("parents") {
        None => Ok(Vec::new()),
        Some(p) if !p.truthy() => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|p| p.as_str().map(str::to_string).ok_or_else(|| RepoError::forward("parent not a str")))
            .collect(),
        Some(_) => Err(RepoError::forward("parents not a list")),
    }
}

/// `root.rglob("*")` keeping files: (posix path relative to root, path).
pub fn walk_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), root.to_path_buf())];
    while let Some((prefix, dir)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| RepoError::forward(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| RepoError::forward(e.to_string()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| RepoError::forward("non-UTF-8 file name"))?;
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| RepoError::forward(e.to_string()))?;
            if ft.is_symlink() {
                return Err(RepoError::forward("symlink under refs"));
            }
            if ft.is_dir() {
                stack.push((rel, path));
            } else if path.is_file() && !rel.ends_with(".lock") {
                out.push((rel, path));
            }
        }
    }
    Ok(out)
}

/// cli.py `_short_oid`.
pub fn short_oid(oid: &str, length: i64) -> String {
    let n = length.max(4) as usize;
    oid.chars().take(n).collect()
}

/// `value[:n]` of a Python str.
pub fn prefix_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// repo.py `same_entry`: equal canonical JSON (None only equals None).
pub fn same_entry(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => json::canonical_bytes(a) == json::canonical_bytes(b),
        _ => false,
    }
}
