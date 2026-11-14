//! A repository, a folder or a server prepared by someone else: what tmiki
//! must not do with it (run its hooks unasked, print its terminal controls,
//! recurse for ever, lose the user's files). Each test is named after the
//! attack it stops.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tmi_store::json::{self, Value};
use tmi_store::store::ObjectStore;

const TIMEOUT: Duration = Duration::from_secs(120);

/// A fresh directory with `work/` (the working tree) and `cfg/` (the
/// user's config home, so trust.json is the test's own).
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("tmi-hostile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        Sandbox { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// `tmiki <args>` in `dir`: (exit code, stdout, stderr).
    fn tmi_in(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tmiki"));
        cmd.args(args)
            .current_dir(dir)
            .env("TMI_CONFIG_HOME", self.root.join("cfg"))
            .env("TMI_AUTHOR_NAME", "ci")
            .env("TMI_AUTHOR_EMAIL", "ci@example.invalid")
            .env("TMI_COMMIT_TIMESTAMP", "2026-01-01T00:00:00+00:00")
            .env("TMI_PROGRESS", "0")
            .env_remove("TMI_HOOKS_TRUST")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in ["http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            cmd.env_remove(var);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("start tmiki");
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                panic!("tmiki {} did not finish", args.join(" "));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
        (out.status.code().unwrap_or(-1), text(&out.stdout), text(&out.stderr))
    }

    fn tmi(&self, args: &[&str]) -> (i32, String, String) {
        self.tmi_in(&self.work(), args, &[])
    }

    /// A `.tmi` laid out by hand, as in a folder someone unpacked: not
    /// made by `tmiki init` on this machine.
    fn unpacked_repo(&self) {
        let repo = self.work().join(".tmi");
        for rel in ["objects", "refs/heads", "refs/tags", "refs/remotes", "logs", "diff", "conflicts", "hooks"] {
            std::fs::create_dir_all(repo.join(rel)).unwrap();
        }
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo.join("config"), "{\"version\":1,\"remotes\":{}}\n").unwrap();
        std::fs::write(repo.join("index"), "{\n  \"entries\": {},\n  \"version\": 1\n}\n").unwrap();
    }

    fn store(&self) -> ObjectStore {
        ObjectStore::new(self.work().join(".tmi").join("objects"))
    }

    /// A commit on main whose one entry `x.clip` has the layer tree `tree`.
    fn commit_layer_tree(&self, tree: &str) {
        let store = self.store();
        let repo_tree = obj(&format!(
            r#"{{"version":1,"kind":"repository","entries":[{{"path":"x.clip","tree_ref":"{tree}","source_digest":"0","source_blob_ref":"{tree}"}}]}}"#
        ));
        let tree_ref = store.write_json("tree", &repo_tree).unwrap();
        let commit = obj(&format!(
            r#"{{"version":1,"tree_ref":"{tree_ref}","parents":[],"author":{{"name":"x","email":"x"}},"timestamp":"2026-01-01T00:00:00+00:00","message":"m"}}"#
        ));
        let oid = store.write_json("commit", &commit).unwrap();
        std::fs::write(self.work().join(".tmi/refs/heads/main"), format!("{oid}\n")).unwrap();
    }

    fn png(&self, name: &str, seed: u8) {
        let rgba: Vec<u8> = (0..32 * 24).flat_map(|i| [seed, (i % 256) as u8, 40, 255]).collect();
        std::fs::write(self.work().join(name), tmi_codec::png_enc::encode(&rgba, 32, 24, None).unwrap()).unwrap();
    }

    fn backups(&self) -> String {
        std::fs::read_to_string(self.work().join(".tmi/backups.json")).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn obj(text: &str) -> Value {
    json::loads(text).unwrap()
}

fn layer_tree(layers: &[String]) -> Value {
    obj(&format!(r#"{{"version":1,"kind":"layer-tree","layers":[{}]}}"#, layers.join(",")))
}

/// A hook that writes `ran` into the working tree (`%TMI_REPO%`).
fn write_hook(work: &Path, name: &str) {
    let hooks = work.join(".tmi").join("hooks");
    if cfg!(windows) {
        std::fs::write(hooks.join(format!("{name}.bat")), "@echo off\r\necho ran> \"%TMI_REPO%\\ran.txt\"\r\n").unwrap();
    } else {
        let p = hooks.join(name);
        std::fs::write(&p, "#!/bin/sh\necho ran > \"$TMI_REPO/ran.txt\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// A2: a chain of layer trees thousands deep in a commit; `ls-layers`
/// recursed on it until the stack overflowed.
#[test]
fn a_layer_tree_thousands_deep_is_refused_not_overflowing_the_stack() {
    let sb = Sandbox::new("deep");
    sb.unpacked_repo();
    let store = sb.store();
    let mut sub = store.write_json("tree", &layer_tree(&[r#"{"name":"leaf","layer_id":"1"}"#.into()])).unwrap();
    for i in 0..5000 {
        sub = store.write_json("tree", &layer_tree(&[format!(r#"{{"name":"f{i}","layer_id":"{i}","subtree_ref":"{sub}"}}"#)])).unwrap();
    }
    sb.commit_layer_tree(&sub);
    let (code, _, err) = sb.tmi(&["ls-layers", "x.clip", "HEAD"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("layer tree too deep"), "{err}");
}

/// A1 through ls-layers: layers that all name the same subtree, three
/// levels of 1100: a billion visits from three small objects.
#[test]
fn a_layer_tree_sharing_one_subtree_many_times_is_refused_not_walked_for_ever() {
    let sb = Sandbox::new("wide");
    sb.unpacked_repo();
    let store = sb.store();
    let leaves: Vec<String> = (0..1100).map(|i| format!(r#"{{"name":"l{i}","layer_id":"{i}"}}"#)).collect();
    let mut sub = store.write_json("tree", &layer_tree(&leaves)).unwrap();
    for _ in 0..2 {
        let folders: Vec<String> = (0..1100).map(|i| format!(r#"{{"name":"f{i}","layer_id":"{i}","subtree_ref":"{sub}"}}"#)).collect();
        sub = store.write_json("tree", &layer_tree(&folders)).unwrap();
    }
    sb.commit_layer_tree(&sub);
    let (code, _, err) = sb.tmi(&["ls-layers", "x.clip", "HEAD"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("layer tree too large"), "{err}");
}

/// A3: a layer name carrying terminal controls (here: set the window title,
/// clear the screen) reached the terminal as it was.
#[test]
fn a_layer_name_with_terminal_controls_is_printed_escaped() {
    let sb = Sandbox::new("escape");
    sb.unpacked_repo();
    let tree = sb.store().write_json("tree", &layer_tree(&[r#"{"name":"L\u001b]0;pwn\u0007\u001b[2J","layer_id":"1"}"#.into()])).unwrap();
    sb.commit_layer_tree(&tree);
    let (code, out, err) = sb.tmi(&["ls-layers", "x.clip", "HEAD"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    assert!(out.contains(r"L\x1b]0;pwn\x07\x1b[2J"), "{out}");
}

/// C15: a server's `{"error": ...}` detail went to the terminal raw through
/// the error line.
#[test]
fn a_servers_error_text_with_terminal_controls_is_printed_escaped() {
    let sb = Sandbox::new("server");
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && s.read(&mut b).map(|n| n == 1).unwrap_or(false) {
                head.push(b[0]);
            }
            let body = r#"{"error":"nope\u001b]0;pwn\u0007\u001b[2J"}"#;
            let _ = write!(
                s,
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let url = format!("http://127.0.0.1:{port}/api/u/r");
    let (code, _, err) = sb.tmi_in(&sb.root, &["clone", &url, "dst"], &[]);
    assert_ne!(code, 0, "{err}");
    assert!(err.contains("nope"), "the server's words are shown: {err}");
    assert!(!err.contains('\u{1b}') && !err.contains('\u{7}'), "{err:?}");
}

/// A5: a folder that came with `.tmi/hooks/pre-commit`: `tmiki commit`
/// in it ran the hook (a program of whoever made the folder).
#[test]
fn a_hook_that_came_with_an_unpacked_repository_does_not_run() {
    let sb = Sandbox::new("unpacked-hook");
    sb.unpacked_repo();
    write_hook(&sb.work(), "pre-commit");
    sb.png("a.png", 1);
    assert_eq!(sb.tmi(&["add", "a.png"]).0, 0);
    let (code, _, err) = sb.tmi(&["commit", "-m", "x"]);
    assert_eq!(code, 1, "the pre-commit hook did not run, so the commit stops: {err}");
    assert!(err.contains("TMI_HOOKS_TRUST=allow"), "says how to allow it: {err}");
    assert!(!sb.work().join("ran.txt").exists(), "the hook ran");
    // allowed for one command: it runs
    let (code, _, err) = sb.tmi_in(&sb.work(), &["commit", "-m", "x"], &[("TMI_HOOKS_TRUST", "allow")]);
    assert_eq!(code, 0, "{err}");
    assert!(sb.work().join("ran.txt").exists());
}

/// A5's other side: a repository made by `tmiki init` here runs its hooks
/// with no question, also after they are edited.
#[test]
fn a_hook_of_a_repository_made_by_init_runs_unasked() {
    let sb = Sandbox::new("init-hook");
    assert_eq!(sb.tmi(&["init"]).0, 0);
    write_hook(&sb.work(), "pre-commit");
    sb.png("a.png", 1);
    assert_eq!(sb.tmi(&["add", "a.png"]).0, 0);
    let (code, _, err) = sb.tmi(&["commit", "-m", "x"]);
    assert_eq!(code, 0, "{err}");
    assert!(sb.work().join("ran.txt").exists());
}

/// A7: `commit --amend` of a pulled commit hands its author's message to
/// the hooks; `%TMI_COMMIT_MSG%` carried its control characters, and the
/// exact text had no safe way in.
#[test]
fn a_commit_message_reaches_hooks_as_a_file_and_without_controls() {
    let sb = Sandbox::new("msg");
    assert_eq!(sb.tmi(&["init"]).0, 0);
    let hooks = sb.work().join(".tmi").join("hooks");
    if cfg!(windows) {
        std::fs::write(
            hooks.join("pre-commit.bat"),
            "@echo off\r\ncopy /b \"%TMI_COMMIT_MSG_FILE%\" \"%TMI_REPO%\\msg.txt\" >nul\r\nset TMI_COMMIT_MSG> \"%TMI_REPO%\\env.txt\"\r\n",
        )
        .unwrap();
    } else {
        let p = hooks.join("pre-commit");
        std::fs::write(&p, "#!/bin/sh\ncp \"$TMI_COMMIT_MSG_FILE\" \"$TMI_REPO/msg.txt\"\nprintenv TMI_COMMIT_MSG > \"$TMI_REPO/env.txt\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    sb.png("a.png", 1);
    assert_eq!(sb.tmi(&["add", "a.png"]).0, 0);
    let message = "fix\u{1b}]0;pwn\u{7} & more";
    let (code, _, err) = sb.tmi(&["commit", "-m", message]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(sb.work().join("msg.txt")).unwrap(), message);
    let env = std::fs::read_to_string(sb.work().join("env.txt")).unwrap();
    assert!(env.contains("fix]0;pwn & more"), "{env:?}");
    assert!(!env.contains('\u{1b}') && !env.contains('\u{7}'), "{env:?}");
}

/// A9: `restore --in-place <file>:<layer>` replaced a working file that had
/// unsaved work, with no backup.
#[test]
fn restoring_a_layer_in_place_keeps_the_unsaved_file_in_the_backups() {
    let sb = Sandbox::new("inplace");
    assert_eq!(sb.tmi(&["init"]).0, 0);
    sb.png("a.png", 1);
    assert_eq!(sb.tmi(&["add", "a.png"]).0, 0);
    assert_eq!(sb.tmi(&["commit", "-m", "x"]).0, 0);
    sb.png("a.png", 99); // unsaved work
    let (code, out, err) = sb.tmi(&["restore", "--in-place", "a.png:a"]);
    assert_eq!(code, 0, "{out}{err}");
    let backups = sb.backups();
    assert!(backups.contains("\"a.png\"") && backups.contains("restore --in-place"), "{backups}");
}

/// A10: a stash that deleted `b.png`, applied where `b.png` is now a new,
/// untracked file: the file was deleted with no backup.
#[test]
fn applying_a_stash_keeps_an_untracked_file_it_deletes_in_the_backups() {
    let sb = Sandbox::new("stash");
    assert_eq!(sb.tmi(&["init"]).0, 0);
    sb.png("a.png", 1);
    sb.png("b.png", 2);
    assert_eq!(sb.tmi(&["add", "a.png", "b.png"]).0, 0);
    assert_eq!(sb.tmi(&["commit", "-m", "x"]).0, 0);
    std::fs::remove_file(sb.work().join("b.png")).unwrap();
    let (code, out, err) = sb.tmi(&["stash"]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = sb.tmi(&["rm", "b.png"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(sb.tmi(&["commit", "-m", "no b"]).0, 0);
    sb.png("b.png", 77); // the user's new work, untracked
    let (code, out, err) = sb.tmi(&["stash", "apply"]);
    assert_eq!(code, 0, "{out}{err}");
    let backups = sb.backups();
    assert!(backups.contains("\"b.png\"") && backups.contains("stash apply"), "{backups}");
}

/// A12: `mv a.png .tmi/hooks/pre-commit.bat` moved the file into the hooks
/// directory before staging refused the path.
#[test]
fn moving_a_file_into_the_hooks_directory_is_refused_before_it_moves() {
    let sb = Sandbox::new("mv");
    assert_eq!(sb.tmi(&["init"]).0, 0);
    sb.png("a.png", 1);
    assert_eq!(sb.tmi(&["add", "a.png"]).0, 0);
    assert_eq!(sb.tmi(&["commit", "-m", "x"]).0, 0);
    let (code, _, err) = sb.tmi(&["mv", "a.png", ".tmi/hooks/pre-commit.bat"]);
    assert_eq!(code, 1, "{err}");
    assert!(sb.work().join("a.png").is_file());
    assert!(!sb.work().join(".tmi/hooks/pre-commit.bat").exists());
}
