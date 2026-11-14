//! A hostile tmihub (tests/test_https_hostile.py has the Python twins):
//! it redirects to another host, wants the token over plain http, sends a
//! reply, an error body or a download that never ends, or sends one byte at
//! a time. tmiki must keep its token, its memory and its disk, and give up.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tmi_transport::https::HttpsRemote;
use tmi_transport::ops;

/// The environment is the process's: one test at a time.
static ENV: Mutex<()> = Mutex::new(());

struct Ctx {
    stop: AtomicBool,
    sent: AtomicU64,
    heads: Mutex<Vec<String>>,
}

struct Srv {
    port: u16,
    ctx: Arc<Ctx>,
    accept: Option<JoinHandle<()>>,
}

impl Srv {
    fn url(&self, host: &str) -> String {
        format!("http://{host}:{}/api/u/r", self.port)
    }
    fn heads(&self) -> Vec<String> {
        self.ctx.heads.lock().unwrap().clone()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.ctx.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

/// Serve `bind`:0; `handler(stream, request line, ctx)` answers each request
/// (the head is read and recorded first).
fn serve<F>(bind: &str, handler: F) -> Srv
where
    F: Fn(&mut TcpStream, &str, &Ctx) + Send + Sync + 'static,
{
    let listener = TcpListener::bind((bind, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let ctx = Arc::new(Ctx { stop: AtomicBool::new(false), sent: AtomicU64::new(0), heads: Mutex::new(Vec::new()) });
    let handler = Arc::new(handler);
    let c = ctx.clone();
    let accept = std::thread::spawn(move || {
        while !c.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let (c, h) = (c.clone(), handler.clone());
                    std::thread::spawn(move || {
                        stream.set_nonblocking(false).unwrap();
                        let Some(head) = read_head(&mut stream) else { return };
                        c.heads.lock().unwrap().push(head.clone());
                        let line = head.lines().next().unwrap_or("").to_string();
                        h(&mut stream, &line, &c);
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
    Srv { port, ctx, accept: Some(accept) }
}

/// The request head (and the body it announces, discarded).
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut data = Vec::new();
    let mut byte = [0u8; 1];
    while !data.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        data.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&data).to_string();
    let length = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    Some(head)
}

fn json_reply(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// A reply of `total` bytes (or until the client leaves) with no length.
fn endless(stream: &mut TcpStream, status: &str, prefix: &[u8], fill: u8, total: u64, ctx: &Ctx) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\n\r\n");
    if stream.write_all(prefix).is_err() {
        return;
    }
    let chunk = vec![fill; 1 << 20];
    while ctx.sent.load(Ordering::SeqCst) < total && !ctx.stop.load(Ordering::SeqCst) {
        if stream.write_all(&chunk).is_err() {
            return;
        }
        ctx.sent.fetch_add(chunk.len() as u64, Ordering::SeqCst);
    }
}

const ENV_NAMES: &[&str] = &[
    "TMIHUB_TOKEN",
    "TMIHUB_HOST",
    "TMIHUB_TIMEOUT",
    "TMIHUB_MAX_REPLY_BYTES",
    "TMIHUB_MAX_DOWNLOAD_BYTES",
    "TMIHUB_PACE_WINDOW",
    "TMIHUB_PACE_MIN_BYTES",
];

/// Run `f` with exactly these tmihub variables set, no proxy and an empty
/// user config; `tmp` is where temporary files go.
fn with_env<T>(vars: &[(&str, &str)], tmp: Option<&PathBuf>, f: impl FnOnce() -> T) -> T {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    for name in ENV_NAMES {
        std::env::remove_var(name);
    }
    let proxies: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| k.to_string_lossy().to_ascii_lowercase().ends_with("_proxy"))
        .map(|(k, _)| k)
        .collect();
    for k in proxies {
        std::env::remove_var(k);
    }
    let cfg = std::env::temp_dir().join(format!("tmi-hostile-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("TMI_CONFIG_HOME", &cfg);
    for (k, v) in vars {
        std::env::set_var(k, v);
    }
    let saved: Vec<_> = ["TMP", "TEMP", "TMPDIR"].iter().map(|k| (*k, std::env::var_os(k))).collect();
    if let Some(dir) = tmp {
        for k in ["TMP", "TEMP", "TMPDIR"] {
            std::env::set_var(k, dir);
        }
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    for name in ENV_NAMES {
        std::env::remove_var(name);
    }
    out
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tmi-hostile-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn refs_err(url: &str) -> String {
    HttpsRemote::new(url).unwrap().list_refs().err().expect("list_refs succeeded").to_string()
}

#[test]
fn a_redirect_is_not_followed_and_the_token_stays_home() {
    let target = serve("127.0.0.1", |s, _, _| json_reply(s, "404 Not Found", r#"{"error":"x"}"#));
    let to = format!("http://127.0.0.1:{}/api/u/r/refs", target.port);
    let hub = serve("127.0.0.1", move |s, _, _| {
        let _ = write!(s, "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    });
    let bound = format!("127.0.0.1:{}", hub.port);
    let err = with_env(&[("TMIHUB_TOKEN", "s3cret"), ("TMIHUB_HOST", &bound)], None, || refs_err(&hub.url("127.0.0.1")));
    assert!(err.contains("redirect (302)") && err.contains("does not follow redirects"), "{err}");
    assert!(err.contains(&format!("127.0.0.1:{}", target.port)), "{err}");
    assert!(hub.heads().iter().all(|h| h.contains("Authorization: Bearer s3cret")), "the hub itself gets the token");
    assert!(target.heads().is_empty(), "the redirect target was contacted: {:?}", target.heads());
}

#[test]
fn the_token_is_not_sent_over_plain_http_to_another_machine() {
    // 127.0.0.2 is this machine, but not the loopback names tmiki trusts
    let hub = serve("127.0.0.2", |s, _, _| json_reply(s, "200 OK", r#"{"refs":{}}"#));
    let bound = format!("127.0.0.2:{}", hub.port);
    let err = with_env(&[("TMIHUB_TOKEN", "s3cret"), ("TMIHUB_HOST", &bound)], None, || refs_err(&hub.url("127.0.0.2")));
    assert!(err.contains("refusing to send the tmihub token over plain http to 127.0.0.2"), "{err}");
    assert!(hub.heads().is_empty(), "something was sent: {:?}", hub.heads());
    // no token: an anonymous read over http is fine
    let refs = with_env(&[], None, || HttpsRemote::new(&hub.url("127.0.0.2")).unwrap().list_refs().unwrap());
    assert!(refs.is_empty());
    assert!(hub.heads().iter().all(|h| !h.contains("Authorization")));
}

#[test]
fn the_token_still_goes_to_the_loopback_hub_it_was_set_for() {
    let hub = serve("127.0.0.1", |s, _, _| json_reply(s, "200 OK", r#"{"refs":{}}"#));
    let bound = format!("http://127.0.0.1:{}", hub.port);
    with_env(&[("TMIHUB_TOKEN", "s3cret"), ("TMIHUB_HOST", &bound)], None, || {
        HttpsRemote::new(&hub.url("127.0.0.1")).unwrap().list_refs().unwrap()
    });
    assert!(hub.heads().iter().all(|h| h.contains("Authorization: Bearer s3cret")), "{:?}", hub.heads());
    assert!(!hub.heads().is_empty());
}

#[test]
fn the_token_is_not_sent_to_a_host_it_was_not_set_for() {
    let hub = serve("127.0.0.1", |s, line, _| {
        if line.contains("/head ") {
            json_reply(s, "401 Unauthorized", r#"{"error":"login"}"#)
        } else {
            json_reply(s, "200 OK", r#"{"refs":{}}"#)
        }
    });
    // a token for another hub, and a token with no hub named
    for host in [Some("hub.example"), None] {
        let mut vars = vec![("TMIHUB_TOKEN", "s3cret")];
        vars.extend(host.map(|h| ("TMIHUB_HOST", h)));
        let remote = with_env(&vars, None, || HttpsRemote::new(&hub.url("127.0.0.1")).unwrap());
        assert!(remote.list_refs().unwrap().is_empty());
        assert!(remote.head_branch().unwrap().is_none());
    }
    assert!(!hub.heads().is_empty());
    assert!(hub.heads().iter().all(|h| !h.contains("s3cret")), "{:?}", hub.heads());
    // the refusal says how to bind it
    let hub401 = serve("127.0.0.1", |s, _, _| json_reply(s, "401 Unauthorized", r#"{"error":"login"}"#));
    let err = with_env(&[("TMIHUB_TOKEN", "s3cret")], None, || refs_err(&hub401.url("127.0.0.1")));
    assert!(err.contains("not for 127.0.0.1:") && err.contains("tmihub.host"), "{err}");
}

#[test]
fn listed_ids_naming_local_paths_or_shares_are_not_probed() {
    let marker = scratch("probe").join("secret.txt");
    std::fs::write(&marker, b"x").unwrap();
    let ids = [format!("ab{}", marker.display()), format!("ab{}", marker.with_extension("absent").display())];
    let body = format!(r#"{{"oids":[{}]}}"#, ids.iter().map(|i| json_str(i)).collect::<Vec<_>>().join(","));
    let hub = serve("127.0.0.1", move |s, _, _| json_reply(s, "200 OK", &body));
    let case = scratch("probe-repo");
    let repo = ops::create(&case.join("repo"), false).unwrap();
    let got = with_env(&[], None, || HttpsRemote::new(&hub.url("127.0.0.1")).unwrap().pull_objects(&repo.store, &|_: &str| {}));
    assert_eq!(got.unwrap(), 0);
    // neither id was asked for: one that exists here would otherwise be left
    // out of the download request and one that does not would be in it
    assert!(hub.heads().iter().all(|h| !h.contains("objects/download")), "{:?}", hub.heads());
    let _ = std::fs::remove_dir_all(&case);
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[test]
fn a_reply_that_never_ends_is_cut_off() {
    let hub = serve("127.0.0.1", |s, _, ctx| endless(s, "200 OK", br#"{"refs": {"refs/heads/main": ""#, b'a', 256 << 20, ctx));
    let err = with_env(&[("TMIHUB_MAX_REPLY_BYTES", "1000000")], None, || refs_err(&hub.url("127.0.0.1")));
    assert!(err.contains("reply larger than 1000000 bytes"), "{err}");
    assert!(hub.ctx.sent.load(Ordering::SeqCst) < 100 << 20, "the client kept reading");
}

#[test]
fn an_error_body_that_never_ends_is_cut_off() {
    let hub = serve("127.0.0.1", |s, _, ctx| endless(s, "500 Internal Server Error", b"{", b'x', 512 << 20, ctx));
    let err = with_env(&[], None, || refs_err(&hub.url("127.0.0.1")));
    assert!(err.contains("tmihub request failed (500)"), "{err}");
    let sent = hub.ctx.sent.load(Ordering::SeqCst);
    assert!(sent < 100 << 20, "the client read {} MiB of an error body", sent >> 20);
}

#[test]
fn a_download_that_never_ends_is_cut_off_and_its_temp_file_removed() {
    let hub = serve("127.0.0.1", |s, _, ctx| endless(s, "200 OK", b"", 0, 512 << 20, ctx));
    let case = scratch("download");
    let repo = ops::create(&case.join("repo"), false).unwrap();
    let tmp = case.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let err = with_env(&[("TMIHUB_MAX_DOWNLOAD_BYTES", "3000000")], Some(&tmp), || {
        HttpsRemote::new(&hub.url("127.0.0.1"))
            .unwrap()
            .fetch_objects(&repo.store, &["ab".repeat(32)], &|_: &str| {})
            .err()
            .expect("fetch succeeded")
            .to_string()
    });
    assert!(err.contains("download larger than 3000000 bytes"), "{err}");
    assert!(hub.ctx.sent.load(Ordering::SeqCst) < 100 << 20, "the client kept reading");
    let left: Vec<_> = std::fs::read_dir(&tmp).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "temporary files left: {left:?}");
    let _ = std::fs::remove_dir_all(&case);
}

#[test]
fn a_reply_trickled_one_byte_at_a_time_is_given_up() {
    let hub = serve("127.0.0.1", |s, _, ctx| {
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n");
        while !ctx.stop.load(Ordering::SeqCst) {
            if s.write_all(b" ").and_then(|()| s.flush()).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let url = hub.url("127.0.0.1");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let err = with_env(&[("TMIHUB_PACE_WINDOW", "1"), ("TMIHUB_PACE_MIN_BYTES", "1024")], None, || refs_err(&url));
        let _ = tx.send((err, started.elapsed()));
    });
    let (err, took) = rx.recv_timeout(Duration::from_secs(30)).expect("tmiki is still waiting for a trickle");
    assert!(err.contains("sending data too slowly"), "{err}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}

#[test]
fn a_slow_but_steady_reply_is_not_given_up() {
    let hub = serve("127.0.0.1", |s, _, _| {
        let body = r#"{"refs":{}}"#;
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        for b in body.bytes() {
            let _ = s.write_all(&[b]);
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    // 11 bytes in about a third of a second: far above the pace asked for
    let refs = with_env(&[("TMIHUB_PACE_WINDOW", "1"), ("TMIHUB_PACE_MIN_BYTES", "1")], None, || {
        HttpsRemote::new(&hub.url("127.0.0.1")).unwrap().list_refs().unwrap()
    });
    assert!(refs.is_empty());
}
