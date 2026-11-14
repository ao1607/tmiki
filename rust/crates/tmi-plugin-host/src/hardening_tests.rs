//! Hostile plugins (the threat model of PLUGINS.md 11: a component or a
//! development plugin is somebody else's code): what a plugin says about
//! itself and what it does to the host's pipes. `rust/plugins/bad-dev` is
//! the misbehaving development plugin (its first argument is the mode).

use std::time::{Duration, Instant};

use tmi_format::types as t;

use crate::dev::{DevCommand, DevOptions, DevProcessBackend};
use crate::plugin::{check_info, is_extension, is_semver};
use crate::cache::Outcome;
use crate::{test_plugins, PluginBackend, PluginError};

fn info(version: &str, extensions: &[&str]) -> t::PluginInfo {
    t::PluginInfo {
        id: "evil".into(),
        version: version.into(),
        formats: vec![t::FormatInfo { name: "Evil".into(), extensions: extensions.iter().map(|e| e.to_string()).collect(), mime: None }],
        probe_bytes: 16,
        can_write: false,
    }
}

fn why(i: &t::PluginInfo) -> String {
    match check_info(i) {
        Err(PluginError::Info(m)) => m,
        other => panic!("{other:?}"),
    }
}

fn start(mode: &str, timeout: Duration) -> Result<DevProcessBackend, PluginError> {
    DevProcessBackend::start(DevCommand::new(test_plugins::bad_dev(), [mode]), DevOptions { timeout: Some(timeout), ..DevOptions::default() })
}

#[test]
fn extensions_that_would_write_attribute_lines_are_refused() {
    assert!(check_info(&info("1.0.0", &["ora", ".ora", "a1", "0"])).is_ok());
    for bad in [".evil\n*.png format=plugin:evil\n*", "*", "o ra", "ora\r", "or=a", "..ora", "", ".", "ora/x", "ORA", "o\u{0}", "\u{202e}ora", "über", &"a".repeat(17)] {
        let m = why(&info("1.0.0", &[bad]));
        assert!(m.contains("extension"), "{bad:?}: {m}");
        // the message itself is one safe line
        assert!(!m.contains('\n') && !m.contains('\r') && !m.contains('\u{1b}'), "{m:?}");
    }
    let many: Vec<String> = (0..65).map(|i| format!("e{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(why(&info("1.0.0", &many)).contains("over the limit"));
    assert!(is_extension(".ora") && !is_extension("..ora"));
}

#[test]
fn a_version_is_a_semver_all_the_way() {
    for good in ["0.1.0", "1.2.3", "1.0.0-rc.1", "1.0.0-rc.1+build.5", "1.0.0+20260101", "10.20.30-alpha-1"] {
        assert!(is_semver(good), "{good}");
        assert!(check_info(&info(good, &["ora"])).is_ok(), "{good}");
    }
    for bad in [
        "1.0.0-rc\r\x1b[2K\x1b[32msigned\x1b[0m",
        "1.0.0-a:b",
        "1.0.0-a/b",
        "1.0.0-a\\b",
        "1.0.0-",
        "1.0.0-a..b",
        "1.0.0+",
        "1.0",
        "1.0.0.0",
        "1.0.0\n",
        "v1.0.0",
        "1.0.0-\u{202e}x",
        &format!("1.0.0-{}", "a".repeat(80)),
        "1.0.0-a b",
    ] {
        assert!(!is_semver(bad), "{bad:?}");
        let m = why(&info(bad, &["ora"]));
        assert!(m.contains("not semver"), "{m}");
        assert!(!m.contains('\r') && !m.contains('\u{1b}') && !m.contains('\n'), "the message carries the plugin's controls: {m:?}");
    }
}

#[test]
fn a_development_plugin_with_a_hostile_info_is_refused() {
    let ok = start("ok", Duration::from_secs(20)).unwrap();
    assert_eq!(ok.info().version, "0.1.0");
    for (mode, word) in [("version-escape", "semver"), ("version-colon", "semver"), ("ext-newline", "extension"), ("ext-glob", "extension"), ("name-escape", "format name")] {
        match start(mode, Duration::from_secs(20)) {
            Err(PluginError::Info(m)) => {
                assert!(m.contains(word), "{mode}: {m}");
                assert!(!m.contains('\u{1b}') && !m.contains('\r') && !m.contains('\n'), "{mode}: {m:?}");
            }
            other => panic!("{mode}: {:?}", other.err()),
        }
    }
}

#[test]
fn a_plugin_error_message_reaches_the_user_without_terminal_controls() {
    let b = start("escape-error", Duration::from_secs(20)).unwrap();
    match b.read_structure("doc.baddev", b"x") {
        Err(PluginError::Read(t::ReadError::Corrupt(m))) => {
            assert!(m.contains("trusted"), "{m}");
            assert!(!m.contains('\u{1b}') && !m.contains('\r') && !m.contains('\u{202e}'), "{m:?}");
            assert!(m.contains("\\x1b[32m") && m.contains("\\r"), "{m:?}");
        }
        other => panic!("{other:?}"),
    }
}

/// A plugin that asks for a large answer and never reads its stdin must not
/// stop tmiki: the send is bound by the call's time limit too.
#[test]
fn a_plugin_that_never_reads_its_stdin_cannot_stop_the_host() {
    let b = std::sync::Arc::new(start("no-read", Duration::from_secs(3)).unwrap());
    let t0 = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = b.clone();
    // detached: a host that is stuck sending must fail this test, not hang it
    std::thread::spawn(move || {
        let source = vec![7u8; 24 << 20];
        let _ = tx.send(worker.read_structure("doc.baddev", &source));
    });
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Err(PluginError::Timeout { limit })) => assert_eq!(limit, Duration::from_secs(3)),
        Ok(other) => panic!("{other:?}"),
        Err(_) => panic!("the host is stuck sending to a plugin that does not read (no time limit on the send), after {:?}", t0.elapsed()),
    }
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
}
// -- the compiled-component cache (a directory others may write) -------------

fn cwasm_of(dir: &std::path::Path, h: &crate::PluginHost, sha: &str) -> std::path::PathBuf {
    dir.join(crate::cache::key(h.engine(), &crate::engine::fingerprint(h.settings()))).join(format!("{sha}.cwasm"))
}

/// A `.cwasm` is native code: one that is not the one tmiki sealed is never
/// loaded, whoever wrote it and however valid it is.
#[test]
fn a_cached_file_that_is_not_the_sealed_one_is_never_loaded() {
    let dir = crate::tests::temp_dir_for_hardening("cache-seal");
    let h = crate::PluginHost::new(crate::Settings { cache_dir: Some(dir.clone()), workers: 1, ..crate::Settings::default() }).unwrap();
    let ora = test_plugins::component("tmi-plugin-ora");
    let bad = test_plugins::component("tmi-plugin-bad");
    let a = h.load(&ora).unwrap();
    let b = h.load(&bad).unwrap();
    assert_eq!((a.cache.clone(), b.cache.clone()), (Outcome::Compiled, Outcome::Compiled));
    let (fa, fb) = (cwasm_of(&dir, &h, &a.sha256), cwasm_of(&dir, &h, &b.sha256));
    let seal = |f: &std::path::Path| f.with_extension("cwasm.seal");
    assert!(seal(&fa).is_file());
    assert_eq!(h.load(&ora).unwrap().cache, Outcome::Loaded);

    // 1. another valid compiled component put in place of the first (the seal is the first's)
    std::fs::copy(&fb, &fa).unwrap();
    assert!(matches!(h.load(&ora).unwrap().cache, Outcome::Replaced(ref m) if m.contains("seal")), "swapped file");
    assert_eq!(h.load(&ora).unwrap().cache, Outcome::Loaded, "written over, sealed again");

    // 2. file and seal both taken from another component's entry
    std::fs::copy(&fb, &fa).unwrap();
    std::fs::copy(seal(&fb), seal(&fa)).unwrap();
    assert!(matches!(h.load(&ora).unwrap().cache, Outcome::Replaced(_)), "another component's file and seal");

    // 3. a seal made by somebody who does not have the key
    let mut s = std::fs::read(seal(&fa)).unwrap();
    s[0] ^= 1;
    std::fs::write(seal(&fa), s).unwrap();
    assert!(matches!(h.load(&ora).unwrap().cache, Outcome::Replaced(_)), "forged seal");

    // 4. no seal at all (a file an older tmiki, or an attacker, left)
    std::fs::remove_file(seal(&fa)).unwrap();
    assert!(matches!(h.load(&ora).unwrap().cache, Outcome::Replaced(ref m) if m.contains("no seal")));

    // 5. one flipped byte inside the file
    let mut f = std::fs::read(&fa).unwrap();
    let mid = f.len() / 2;
    f[mid] ^= 0xff;
    std::fs::write(&fa, f).unwrap();
    assert!(matches!(h.load(&ora).unwrap().cache, Outcome::Replaced(_)), "tampered bytes");
    assert_eq!(h.load(&ora).unwrap().cache, Outcome::Loaded);
    let _ = std::fs::remove_dir_all(&dir);
}