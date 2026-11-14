//! A hostile remote writes trees tmiki's own commands never write: a path that
//! leaves the working tree (`../x`, `a/../../x`, a drive, a UNC path), one
//! that lands in `.tmi` (`.tmi/hooks/post-commit.bat`, `.TMI`, `TMI~1`), a
//! Windows device name, a `:`, a trailing dot. Every one must be refused when
//! it is received (clone, fetch: before a ref moves) and again when it is
//! written to a working tree (switch, restore_all: before anything is
//! written), with nothing created outside the clone and nothing in `.tmi`.
//! The Python side's twins are tests/test_unsafe_paths.py; the conformance
//! scenarios `unsafe-paths` and `transport-unsafe-paths` compare both.

use std::path::{Path, PathBuf};
use tmi_repo::Repo;
use tmi_store::json::{self, Value};
use tmi_transport::ops;

const ATTACKS: &[&str] = &[
    "../escape.txt",
    "sub/../../escape2.txt",
    ".tmi/hooks/post-commit.bat",
    ".TMI/hooks/post-commit.bat",
    ".tmi./hooks/post-commit.bat",
    "TMI~1/hooks/post-commit.bat",
    "sub/.tmi/hooks/post-commit.bat",
    ".git/config",
    "c:/escape.txt",
    "c:escape.txt",
    "//server/share/escape.txt",
    "\\\\server\\share\\escape.txt",
    "sub\\..\\..\\escape.txt",
    "/escape.txt",
    "CON",
    "sub/nul.txt",
    "COM1.png",
    "file.txt:stream",
    "trailing.",
    "trailing /x",
    "a//b",
    "",
];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tmi-unsafe-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn parse(text: String) -> Value {
    json::loads(&text).unwrap()
}

fn jstr(s: &str) -> String {
    json::dumps(&Value::Str(s.to_string()), &json::DumpOptions::CANONICAL)
}

/// A commit on `branch` whose tree records `paths` (each an opaque file),
/// written straight into the object store as a server would.
fn forge(repo: &Repo, branch: &str, paths: &[&str]) -> String {
    let blob = repo.store.write("blob", b"payload").unwrap();
    let digest = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
    let entries: Vec<String> = paths
        .iter()
        .map(|p| format!(r#"{{"path":{},"source_blob_ref":"{blob}","source_digest":"{digest}","source_format":"binary"}}"#, jstr(p)))
        .collect();
    let tree = repo
        .store
        .write_json("tree", &parse(format!(r#"{{"entries":[{}],"kind":"repository","version":1}}"#, entries.join(","))))
        .unwrap();
    let commit = repo
        .store
        .write_json("commit", &parse(format!(r#"{{"message":"forged","parents":[],"tree_ref":"{tree}"}}"#)))
        .unwrap();
    repo.write_ref(&format!("refs/heads/{branch}"), &commit).unwrap();
    commit
}

fn tree_of(repo: &Repo, commit: &str) -> String {
    repo.read_commit(commit).unwrap().get("tree_ref").and_then(|t| t.as_str()).unwrap().to_string()
}

fn files_outside_tmi(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if e.file_name() == ".tmi" {
                continue;
            }
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn hooks_empty(repo_root: &Path) -> bool {
    let hooks = repo_root.join(".tmi").join("hooks");
    std::fs::read_dir(&hooks).map(|mut d| d.next().is_none()).unwrap_or(true)
}

fn url(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

#[test]
fn clone_refuses_every_attack() {
    for (i, attack) in ATTACKS.iter().enumerate() {
        let case = scratch(&format!("clone{i}"));
        let src = ops::create(&case.join("src"), false).unwrap();
        let commit = forge(&src, "main", &["good.txt", attack]);
        let dst = case.join("work").join("dst");
        std::fs::create_dir_all(case.join("work")).unwrap();
        let err = ops::clone(&url(&case.join("src")), Some(&url(&dst)), &|_: &str| {}, &mut || {})
            .err()
            .unwrap_or_else(|| panic!("clone accepted {attack:?}"))
            .to_string();
        assert!(err.contains("invalid path"), "{attack:?}: {err}");
        // nothing outside the clone, nothing but tmiki's own files inside
        assert!(files_outside_tmi(&case.join("work")).is_empty(), "{attack:?}: {:?}", files_outside_tmi(&case.join("work")));
        assert!(!case.join("escape.txt").exists() && !case.join("escape2.txt").exists());
        assert!(hooks_empty(&dst), "{attack:?}: a hook was planted");
        let repo = Repo::open_at(&dst).unwrap();
        assert!(repo.read_ref("refs/heads/main").unwrap().is_none(), "{attack:?}: a branch was made");
        assert!(!repo.store.exists(&tree_of(&src, &commit)), "{attack:?}: the tree was stored");
        let _ = std::fs::remove_dir_all(&case);
    }
}

#[test]
fn fetch_refuses_before_a_ref_moves() {
    for (i, attack) in ATTACKS.iter().enumerate() {
        let case = scratch(&format!("fetch{i}"));
        let src = ops::create(&case.join("src"), false).unwrap();
        let first = forge(&src, "main", &["good.txt"]);
        let dst_path = case.join("dst");
        ops::clone(&url(&case.join("src")), Some(&url(&dst_path)), &|_: &str| {}, &mut || {}).unwrap();
        let before = files_outside_tmi(&dst_path);
        let bad = forge(&src, "main", &["good.txt", attack]);
        let dst = Repo::open_at(&dst_path).unwrap();
        let err = ops::fetch(&dst, "origin", &|_: &str| {}).err().unwrap_or_else(|| panic!("fetch accepted {attack:?}")).to_string();
        assert!(err.contains("invalid path"), "{attack:?}: {err}");
        assert_eq!(dst.read_ref("refs/remotes/origin/main").unwrap().as_deref(), Some(first.as_str()), "{attack:?}");
        assert!(!dst.store.exists(&tree_of(&src, &bad)), "{attack:?}: the tree was stored");
        assert_eq!(files_outside_tmi(&dst_path), before);
        assert!(files_outside_tmi(&case).iter().all(|f| f.starts_with("dst/") || f.starts_with("src/")), "{attack:?}");
        assert!(hooks_empty(&dst_path));
        // a pull stops the same way and writes nothing
        assert!(ops::pull(&dst, "origin", None, &|_: &str| {}, &mut Vec::new(), &mut false, &mut || {}).is_err());
        assert_eq!(files_outside_tmi(&dst_path), before);
        let _ = std::fs::remove_dir_all(&case);
    }
}

#[test]
fn a_tree_already_in_the_store_is_refused_when_written() {
    // as left by a version that did not check what it received
    for (i, attack) in ATTACKS.iter().enumerate() {
        let case = scratch(&format!("store{i}"));
        let repo = ops::create(&case.join("repo"), false).unwrap();
        let first = forge(&repo, "main", &["good.txt"]);
        repo.set_head_ref("refs/heads/main").unwrap();
        repo.restore_all(&first, None, "test").unwrap();
        let evil = forge(&repo, "evil", &["new.txt", attack]);
        let before = files_outside_tmi(&case.join("repo"));
        assert_eq!(before, vec!["good.txt".to_string()]);
        let head_before = repo.head_target().unwrap();

        let err = repo.switch("evil", false, &mut || {}).err().unwrap_or_else(|| panic!("switch accepted {attack:?}")).to_string();
        assert!(err.contains("invalid path in a tree"), "{attack:?}: {err}");
        assert_eq!(repo.head_target().unwrap(), head_before, "{attack:?}: HEAD moved");
        let err = repo.restore_all(&evil, None, "test").err().unwrap().to_string();
        assert!(err.contains("invalid path in a tree"), "{attack:?}: {err}");
        let mut warnings = Vec::new();
        assert!(repo.checkout_detached(&evil, true, &mut warnings).is_err());
        assert!(repo.reset(Some(&evil), "hard", &mut warnings).is_err());
        assert_eq!(repo.head_target().unwrap(), head_before, "{attack:?}: HEAD moved");

        assert_eq!(files_outside_tmi(&case.join("repo")), before, "{attack:?}: the working tree changed");
        assert!(files_outside_tmi(&case).iter().all(|f| f.starts_with("repo/")), "{attack:?}: a file outside");
        assert!(hooks_empty(&case.join("repo")), "{attack:?}: a hook was planted");
        let _ = std::fs::remove_dir_all(&case);
    }
}

#[test]
fn legitimate_names_are_accepted() {
    let case = scratch("legit");
    let repo = ops::create(&case.join("repo"), false).unwrap();
    let names = ["a.png", "画像/空.png", "art/new.mdp", "dir with space/x y.clip", ".tmiattributes", ".tmiplugins/p.wasm", "-x.png"];
    let commit = forge(&repo, "main", &names);
    repo.set_head_ref("refs/heads/main").unwrap();
    repo.restore_all(&commit, None, "test").unwrap();
    for n in names {
        assert!(case.join("repo").join(n).is_file(), "{n}");
    }
    let _ = std::fs::remove_dir_all(&case);
}

#[test]
fn a_link_on_the_way_is_not_written_through() {
    let case = scratch("link");
    let repo = ops::create(&case.join("repo"), false).unwrap();
    let outside = case.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let link = case.join("repo").join("d");
    #[cfg(windows)]
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &link.to_string_lossy(), &outside.to_string_lossy()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
    if !made {
        eprintln!("cannot make a link here; skipped");
        return;
    }
    let commit = forge(&repo, "main", &["d/new.txt"]);
    repo.set_head_ref("refs/heads/main").unwrap();
    let err = repo.restore_all(&commit, None, "test").err().expect("written through a link").to_string();
    assert!(err.contains("symbolic link or junction"), "{err}");
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "a file was written through the link");
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "rmdir", &link.to_string_lossy()]).output();
    #[cfg(unix)]
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&case);
}
