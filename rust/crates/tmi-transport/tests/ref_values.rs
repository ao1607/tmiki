//! What a remote lists as refs is data other people write: a value that is not
//! a 64-digit lower-case hex object id (it would reach ref files, messages and
//! the pre-push hook's `TMI_REMOTE_COMMIT`) and a name tmi could not write
//! (a bidi control, a control character, a Windows device name) refuse the
//! whole operation before any ref is written. The Python side's twins are in
//! tests/test_remote_refs.py.

use std::path::{Path, PathBuf};
use tmi_repo::Repo;
use tmi_transport::ops;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tmi-refvals-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn url(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// A ref file as a hostile server would leave it (no check of the name or the value).
fn raw_ref(repo: &Repo, name: &str, value: &str) {
    let path = repo.repo_dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{value}\n")).unwrap();
}

const BAD_VALUES: &[&str] = &[
    "abc\"&calc&\"",
    "\u{1b}]0;pwned\u{7}",
    "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
    "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
];

#[test]
fn a_bad_object_id_refuses_clone_fetch_and_push_before_anything_is_written() {
    for (i, bad) in BAD_VALUES.iter().enumerate() {
        let case = scratch(&format!("val{i}"));
        let src = ops::create(&case.join("src"), false).unwrap();
        let good = src.store.write("blob", b"payload").unwrap();
        raw_ref(&src, "refs/heads/aaa", &good);
        raw_ref(&src, "refs/heads/zzz", bad);

        // clone: no ref of any kind
        let dst = case.join("dst");
        let err = ops::clone(&url(&case.join("src")), Some(&url(&dst)), &|_: &str| {}, &mut || {}).err().unwrap_or_else(|| panic!("clone accepted {bad:?}")).to_string();
        assert!(err.contains("invalid object id"), "{bad:?}: {err}");
        assert!(!err.contains('\u{1b}') && !err.contains('\u{7}'), "{bad:?}: raw control character in {err:?}");
        let repo = Repo::open_at(&dst).unwrap();
        assert!(repo.read_ref("refs/remotes/origin/aaa").unwrap().is_none(), "{bad:?}: a ref was written");
        assert!(repo.read_ref("refs/heads/aaa").unwrap().is_none());

        // fetch
        let fetcher = ops::create(&case.join("fetcher"), false).unwrap();
        fetcher.add_remote("origin", &url(&case.join("src"))).unwrap();
        let err = ops::fetch(&fetcher, "origin", &|_: &str| {}).err().unwrap_or_else(|| panic!("fetch accepted {bad:?}")).to_string();
        assert!(err.contains("invalid object id"), "{bad:?}: {err}");
        assert!(fetcher.read_ref("refs/remotes/origin/aaa").unwrap().is_none(), "{bad:?}: a ref was written");

        // push: the remote's value for the pushed branch is the bad one
        raw_ref(&fetcher, "refs/heads/zzz", &good);
        let err = ops::push(&fetcher, "origin", Some("zzz"), true, false, &|_: &str| {}, &mut || {}).err().unwrap_or_else(|| panic!("push accepted {bad:?}")).to_string();
        assert!(err.contains("invalid object id"), "{bad:?}: {err}");
        let _ = std::fs::remove_dir_all(&case);
    }
}

#[test]
fn a_bad_ref_name_refuses_the_whole_fetch() {
    for (i, name) in ["refs/heads/a\u{202e}b", "refs/heads/x\u{2066}y", "refs/tags/t\u{200f}", "refs/heads/con", "refs/heads/dir/NUL.txt", "refs/heads/lpt1"].iter().enumerate() {
        let case = scratch(&format!("name{i}"));
        let src = ops::create(&case.join("src"), false).unwrap();
        let good = src.store.write("blob", b"payload").unwrap();
        raw_ref(&src, "refs/heads/aaa", &good);
        let dst = case.join("dst");
        ops::clone(&url(&case.join("src")), Some(&url(&dst)), &|_: &str| {}, &mut || {}).unwrap();
        let repo = Repo::open_at(&dst).unwrap();
        // the first listing is clean; the second carries the bad name
        let path = src.repo_dir.join(name);
        let written = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, format!("{good}\n")));
        if written.is_err() || !path.is_file() {
            // this file system cannot hold the name (a device name): the unit test covers it
            let _ = std::fs::remove_dir_all(&case);
            continue;
        }
        raw_ref(&src, "refs/heads/aab", &good);
        let err = ops::fetch(&repo, "origin", &|_: &str| {}).err().unwrap_or_else(|| panic!("fetch accepted {name:?}")).to_string();
        assert!(err.contains("invalid ref name"), "{name:?}: {err}");
        assert!(!err.contains('\u{202e}') && !err.contains('\u{2066}') && !err.contains('\u{200f}'), "{name:?}: {err}");
        assert!(repo.read_ref("refs/remotes/origin/aab").unwrap().is_none(), "{name:?}: all-or-nothing broken");
        let _ = std::fs::remove_dir_all(&case);
    }
}

#[test]
fn check_refs_rules() {
    use tmi_transport::{check_refs, Refs};
    let ok = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut refs = Refs::new();
    refs.insert("refs/heads/main".into(), ok.into());
    refs.insert("refs/heads/empty".into(), String::new());
    assert!(check_refs(refs).is_ok());
    for name in ["refs/heads/CON", "refs/heads/nul.txt", "refs/tags/x/COM1", "refs/heads/a\u{1b}b", "refs/heads/a\u{85}b", "refs/heads/a\u{7f}b", "refs/heads/a\u{202e}b", "refs/heads/a\u{2069}"] {
        let mut refs = Refs::new();
        refs.insert(name.into(), ok.into());
        assert!(check_refs(refs).is_err(), "{name:?}");
    }
}
