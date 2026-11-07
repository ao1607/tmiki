//! `tmiki add` / `commit` of a PNG on a one- and a two-thread rayon pool, the
//! way CI runs them (stdout and stderr piped, no stdin). They hung on
//! two-vCPU runners: the composite and the thumbnail PNG are encoded side
//! by side on pool workers, and mtpng queued its jobs on the same pool and
//! blocked the workers until those jobs finished, so with every worker
//! encoding none was left to run them (tmi_codec::png_enc::pool).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(60);

/// `Repo.create`: the layout `tmiki init` writes (init itself is Python's).
fn init(dir: &Path) {
    let repo = dir.join(".tmi");
    for rel in ["objects", "refs/heads", "refs/tags", "refs/remotes", "logs", "diff", "conflicts", "hooks"] {
        std::fs::create_dir_all(repo.join(rel)).unwrap();
    }
    std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo.join("config"), "{\"version\":1,\"remotes\":{}}\n").unwrap();
    std::fs::write(repo.join("index"), "{\n  \"entries\": {},\n  \"version\": 1\n}\n").unwrap();
}

/// Runs `tmi <args>` in `dir` and returns (exit code, stdout, stderr);
/// fails the test if it does not finish within TIMEOUT.
fn tmi(dir: &Path, python: &Path, threads: usize, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tmiki"))
        .args(args)
        .current_dir(dir)
        .env("RAYON_NUM_THREADS", threads.to_string())
        .env("TMI_AUTHOR_NAME", "ci")
        .env("TMI_AUTHOR_EMAIL", "ci@example.invalid")
        .env("TMI_COMMIT_TIMESTAMP", "2026-01-01T00:00:00+00:00")
        // A Python with no plugins and no tmiki: a forward fails instead of
        // passing, so the commands must be served natively.
        .env("TMI_PYTHON", python)
        .env("TMI_DEBUG_FORWARD", "1")
        .env_remove("TMI_NATIVE")
        .env_remove("TMI_PROGRESS")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start tmi");
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("tmi {} hung on a {threads}-thread pool", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    (out.status.code().unwrap_or(-1), text(&out.stdout), text(&out.stderr))
}

#[test]
fn add_and_commit_finish_on_small_pools() {
    for threads in [1, 2] {
        let tmp = std::env::temp_dir().join(format!("tmi-small-pool-{}-{threads}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // A venv without site-packages content: no tmi.plugins entry points.
        let prefix = tmp.join("py");
        std::fs::create_dir_all(prefix.join("Lib").join("site-packages")).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(prefix.join("pyvenv.cfg"), "include-system-site-packages = false\n").unwrap();
        let python = prefix.join("bin").join(if cfg!(windows) { "python.exe" } else { "python3" });
        init(&work);
        let rgba: Vec<u8> = (0..64 * 48).flat_map(|i| [200, (i % 256) as u8, 40, 255]).collect();
        std::fs::write(work.join("a.png"), tmi_codec::png_enc::encode(&rgba, 64, 48, None).unwrap()).unwrap();

        let (code, out, err) = tmi(&work, &python, threads, &["add", "a.png"]);
        assert_eq!((code, out.as_str(), err.as_str()), (0, "staged a.png\n", ""), "add on {threads} threads");
        let (code, out, err) = tmi(&work, &python, threads, &["commit", "-m", "first"]);
        assert_eq!((code, err.as_str()), (0, ""), "commit on {threads} threads: {out}");
        assert!(out.starts_with("[main ") && out.ends_with("] first\n"), "{out}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
