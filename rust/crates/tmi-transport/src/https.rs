//! transport/https.py `HttpsTransport`: tmihub's object API (`refs`,
//! `head`, `objects/list`, `objects/download`, `objects/upload`,
//! `update_ref`) with a Bearer token, in batches.
//!
//! Requests, batches, progress messages and error texts are Python's.
//!
//! Like urllib, every request opens its own connection, and the timeout
//! (https.py `TIMEOUT`, env `TMIHUB_TIMEOUT`) bounds each step without
//! progress: the connect, each send and each receive. A request that has
//! reached the server is never repeated in Python: a failure from then on
//! (a timeout, a connection closed mid-response) is reported here, in the
//! words Python's exception would print. Only the first request of a run
//! may hand the command over, when it could not even connect (refused, DNS,
//! TLS; not a timeout) and nothing has been written: urllib words that
//! failure, and Python's TLS may accept what rustls refuses. A proxy urllib
//! would use (`proxy::urllib_proxy`) sends the command to Python before the
//! first request.

use crate::{rawobj, zipio, Progress, Refs};
use rayon::prelude::*;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tmi_repo::{RepoError, Result};
use tmi_store::json::{self, DumpOptions, Map, Value};
use tmi_store::ObjectStore;

/// https.py `BATCH_MAX_OBJECTS` / `BATCH_MAX_BYTES`.
pub const BATCH_MAX_OBJECTS: i128 = 256;
pub const BATCH_MAX_BYTES: i128 = 64 * 1024 * 1024;

/// https.py `TIMEOUT`: seconds a connect, send or receive may make no
/// progress (`urlopen(timeout=)`, the socket timeout).
pub const TIMEOUT: i128 = 300;

/// The most JSON (refs, object lists, `head`) tmi reads from one reply, and
/// the most of an error reply it keeps (env `TMIHUB_MAX_REPLY_BYTES`).
pub const MAX_REPLY_BYTES: i128 = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: u64 = 64 * 1024;
/// The most one download request may deliver when its length is not known
/// beforehand (env `TMIHUB_MAX_DOWNLOAD_BYTES`).
pub const MAX_DOWNLOAD_BYTES: i128 = 4 * 1024 * 1024 * 1024;
/// A body is abandoned when it brings fewer than `PACE_MIN_BYTES` bytes in
/// `PACE_WINDOW` seconds (env `TMIHUB_PACE_MIN_BYTES` / `TMIHUB_PACE_WINDOW`).
pub const PACE_WINDOW: i128 = 60;
pub const PACE_MIN_BYTES: i128 = 1024;

/// Whether a request of this run has reached the server (sent anything
/// past the connection). From then on nothing goes to Python.
static TALKED: AtomicBool = AtomicBool::new(false);

pub struct HttpsRemote {
    pub url: String,
    /// The token, only when it was set for this remote's host.
    token: Option<String>,
    /// A token is set, but for another host (or none was named).
    unbound_token: bool,
    /// The scheme, host and port the token is for: the remote URL's.
    origin: Origin,
    limits: Limits,
    agent: ureq::Agent,
}

/// What a reply may cost: bytes to hold, bytes to receive, and the slowest
/// pace worth waiting for.
#[derive(Clone, Copy)]
struct Limits {
    reply: u64,
    download: u64,
    window: Duration,
    min_bytes: u64,
}

/// `(scheme, host, port, netloc)` of a URL, as `urlsplit` gives them
/// (scheme and host lowercased, the port defaulted).
#[derive(PartialEq, Eq, Debug)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
    netloc: String,
}

fn url_origin(url: &str) -> Origin {
    let (scheme, rest) = url.split_once("://").map_or(("", ""), |(s, r)| (s, r));
    let scheme = scheme.to_ascii_lowercase();
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("").to_string();
    let (host, port) = match netloc.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, after)) => (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (v6.to_string(), None),
        },
        None => match netloc.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (netloc.clone(), None),
        },
    };
    let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
    Origin { scheme, host: host.to_ascii_lowercase(), port, netloc }
}

/// A host the token may be sent to without TLS: this machine.
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Why the token is not sent over plain http (https.py `plain_http_token`).
fn plain_http_token(netloc: &str) -> String {
    format!(
        "refusing to send the tmihub token over plain http to {netloc}; use an https:// remote URL, \
         or remove the token (TMIHUB_TOKEN, 'tmiki config --global --unset tmihub.token')"
    )
}

/// https.py `too_big_reply`.
fn too_big_reply(limit: u64) -> String {
    format!("tmihub sent a reply larger than {limit} bytes; tmi will not read it (TMIHUB_MAX_REPLY_BYTES raises the limit)")
}

/// https.py `too_big_download`.
fn too_big_download(limit: u64) -> String {
    format!(
        "tmihub sent a download larger than {limit} bytes; tmi stopped receiving it (TMIHUB_MAX_DOWNLOAD_BYTES raises the limit)"
    )
}

/// https.py `too_slow`.
fn too_slow_text(min_bytes: u64, window: Duration) -> String {
    format!(
        "tmihub is sending data too slowly (under {min_bytes} bytes in {} seconds); tmi gave up \
         (TMIHUB_PACE_WINDOW and TMIHUB_PACE_MIN_BYTES set the limit); run the command again",
        window.as_secs()
    )
}

/// https.py `redirected`: the Location is shown as printable ASCII
/// and at most 200 characters.
fn redirect_text(code: u16, location: Option<&str>) -> String {
    let shown: String = location.unwrap_or("").chars().filter(|c| (' '..='~').contains(c)).take(200).collect();
    if shown.is_empty() {
        format!("tmihub answered with a redirect ({code}); tmi does not follow redirects (check the remote URL)")
    } else {
        format!("tmihub answered with a redirect ({code}) to {shown}; tmi does not follow redirects (check the remote URL)")
    }
}

/// `_env_limit(name, default)`.
fn env_limit(name: &str, default: i128) -> i128 {
    match std::env::var(name).ok().and_then(|v| crate::py_int(&v)) {
        Some(v) if v > 0 => v,
        _ => default,
    }
}

/// `_batches(oids, size_of)`.
pub fn batches(oids: &[String], size_of: Option<&dyn Fn(&str) -> Result<i128>>) -> Result<Vec<Vec<String>>> {
    let max_objects = env_limit("TMIHUB_BATCH_OBJECTS", BATCH_MAX_OBJECTS);
    let max_bytes = env_limit("TMIHUB_BATCH_BYTES", BATCH_MAX_BYTES);
    let mut out = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes: i128 = 0;
    for oid in oids {
        let size = match size_of {
            Some(f) => f(oid)?,
            None => 0,
        };
        if !batch.is_empty() && (batch.len() as i128 >= max_objects || batch_bytes + size > max_bytes) {
            out.push(std::mem::take(&mut batch));
            batch_bytes = 0;
        }
        batch.push(oid.clone());
        batch_bytes += size;
    }
    if !batch.is_empty() {
        out.push(batch);
    }
    Ok(out)
}

/// `_resolve_token()`: the token and the host it is for (`TMIHUB_HOST` with
/// `TMIHUB_TOKEN`, else `tmihub.host` beside `tmihub.token`).
fn resolve_token() -> Option<(String, Option<String>)> {
    if let Some(t) = std::env::var("TMIHUB_TOKEN").ok().filter(|t| !t.is_empty()) {
        let host = std::env::var("TMIHUB_HOST").ok().map(|h| py_strip(&h).to_string()).filter(|h| !h.is_empty());
        return Some((py_strip(&t).to_string(), host));
    }
    let cfg = tmi_repo::config::read_user_config().ok()?;
    let value = tmi_repo::config::get_value(&cfg, "tmihub.token").ok()??;
    if !value.truthy() {
        return None;
    }
    let host = match tmi_repo::config::get_value(&cfg, "tmihub.host") {
        Ok(Some(h)) if h.truthy() => h.py_str().map(|h| py_strip(&h).to_string()),
        _ => None,
    };
    Some((py_strip(&value.py_str()?).to_string(), host))
}

/// Whether the token's host (`host[:port]` or a URL) names the remote's
/// host and port. A token is one person's key to one hub: a remote URL on
/// any other host (a link from someone else) must not receive it.
fn token_host_matches(host: &str, remote: &Origin) -> bool {
    let bound = if host.contains("://") { url_origin(host) } else { url_origin(&format!("{}://{host}", remote.scheme)) };
    !bound.host.is_empty() && bound.host == remote.host && bound.port == remote.port
}

/// Why a set token was not sent to this remote.
fn unbound_token_text(netloc: &str) -> String {
    format!(
        "a tmihub token is set, but not for {netloc}, so it was not sent; if this is your hub, \
         run 'tmiki config --global tmihub.host {netloc}' (TMIHUB_HOST with TMIHUB_TOKEN)"
    )
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Why urllib would send requests for `url` through a proxy (Forward then).
fn urllib_proxy(url: &str) -> Option<String> {
    let vars: Vec<_> = std::env::vars_os().collect();
    #[cfg(windows)]
    let registry: &dyn Fn() -> Vec<String> = &crate::proxy::system_registry_protocols;
    #[cfg(not(windows))]
    let registry: &dyn Fn() -> Vec<String> = &|| Vec::new();
    crate::proxy::urllib_proxy(url, &vars, crate::proxy::System::current(), registry)
}

/// What `_open` raises, by kind.
enum Failure {
    /// urllib.error.HTTPError: the status and the `{"error": ...}` detail.
    Status(u16, String),
    /// A 3xx answer, not followed: the sentence naming where it pointed.
    Redirect(String),
    /// Anything else: the text Python's exception prints, whether urllib
    /// raised it as a URLError (`head_branch` swallows those), and whether
    /// the command may still go to Python.
    Other { text: String, url_error: bool, forward: bool },
}

/// The OS error under a ureq or io error, if any.
fn os_error<'a>(mut e: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    loop {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.raw_os_error().is_some() {
                return Some(io);
            }
            match io.get_ref() {
                Some(inner) => {
                    e = inner;
                    continue;
                }
                None => return None,
            }
        }
        e = e.source()?;
    }
}

/// Whether a ureq or io error is a socket timeout.
fn timed_out(mut e: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                return true;
            }
            if let Some(inner) = io.get_ref() {
                e = inner;
                continue;
            }
        }
        match e.source() {
            Some(s) => e = s,
            None => return false,
        }
    }
}

/// `str(OSError)` for an OS error: "[WinError N] text" (FormatMessage's
/// text without trailing dots and spaces) or "[Errno N] text".
fn py_os_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let Some(code) = e.raw_os_error() else { return text };
    let msg = text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text);
    if cfg!(windows) {
        format!("[WinError {code}] {}", msg.trim_end_matches(|c: char| c <= ' ' || c == '.'))
    } else {
        format!("[Errno {code}] {msg}")
    }
}

/// What Python prints for an exception raised while receiving a response
/// (socket.timeout, ConnectionResetError, ...: not URLError).
fn receive_text(e: &(dyn std::error::Error + 'static)) -> String {
    if timed_out(e) {
        return "timed out".into();
    }
    match os_error(e) {
        Some(io) => py_os_text(io),
        None => e.to_string(),
    }
}

/// ureq's error for a body shorter than its Content-Length; http.client's
/// `read(amt)` just ends there (copyfileobj stops, the zip is then short).
const SHORT_BODY: &str = "response body closed before all bytes were read";

/// A response body read as http.client reads it in chunks, given up when it
/// comes too slowly.
struct Body {
    inner: Box<dyn Read + Send + Sync>,
    window: Duration,
    min_bytes: u64,
    since: Instant,
    got: u64,
}

/// The error `Body` raises for a body that comes too slowly.
#[derive(Debug)]
struct TooSlow(String);

impl std::fmt::Display for TooSlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TooSlow {}

impl Body {
    fn new(inner: Box<dyn Read + Send + Sync>, limits: &Limits) -> Body {
        Body { inner, window: limits.window, min_bytes: limits.min_bytes, since: Instant::now(), got: 0 }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = match self.inner.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && e.to_string() == SHORT_BODY => return Ok(0),
            r => r?,
        };
        self.got += n as u64;
        if n > 0 && self.since.elapsed() >= self.window {
            if self.got < self.min_bytes {
                return Err(std::io::Error::other(TooSlow(too_slow_text(self.min_bytes, self.window))));
            }
            self.since = Instant::now();
            self.got = 0;
        }
        Ok(n)
    }
}

/// A failure reading a body (after the request reached the server).
fn body_error(e: std::io::Error) -> RepoError {
    if let Some(slow) = e.get_ref().and_then(|i| i.downcast_ref::<TooSlow>()) {
        return RepoError::runtime(slow.0.clone());
    }
    RepoError::runtime(receive_text(&e))
}

/// `response.read()`: the whole body, or `IncompleteRead` when it ends
/// before its Content-Length.
fn read_all(resp: ureq::Response, limits: &Limits) -> Result<Vec<u8>> {
    let expected: Option<u64> = resp.header("Content-Length").and_then(|v| v.trim().parse().ok());
    if expected.is_some_and(|n| n > limits.reply) {
        return Err(RepoError::runtime(too_big_reply(limits.reply)));
    }
    let mut bytes = Vec::new();
    let mut body = Body::new(resp.into_reader(), limits);
    (&mut body).take(limits.reply + 1).read_to_end(&mut bytes).map_err(body_error)?;
    if bytes.len() as u64 > limits.reply {
        return Err(RepoError::runtime(too_big_reply(limits.reply)));
    }
    match expected {
        Some(n) if (bytes.len() as u64) < n => Err(RepoError::runtime(format!(
            "IncompleteRead({} bytes read, {} more expected)",
            bytes.len(),
            n - bytes.len() as u64
        ))),
        _ => Ok(bytes),
    }
}

/// A spooled temporary file (`tempfile.TemporaryFile` / `mkstemp`),
/// removed when dropped.
struct Spool {
    path: PathBuf,
    file: std::fs::File,
}

impl Spool {
    fn new(suffix: &str) -> Result<Spool> {
        let dir = std::env::temp_dir();
        for attempt in 0..100u32 {
            let path = dir.join(format!("tmp{}{suffix}", random_hex(attempt, 8)));
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                // objects in transit are the user's: not readable in a shared /tmp
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(file) => return Ok(Spool { path, file }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(RepoError::runtime(format!("{}: {e}", path.display()))),
            }
        }
        Err(RepoError::runtime("no usable temporary file name"))
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn random_hex(salt: u32, n: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    let mut i = 0u32;
    while out.len() < n {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        h.write_u32(salt);
        h.write_u32(i);
        out.push_str(&format!("{:016x}", h.finish()));
        i += 1;
    }
    out.truncate(n);
    out
}

/// Python's type name of a JSON value (for AttributeError texts).
fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `data.get(key, default)` on a decoded response.
fn get_or<'a>(data: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    match data {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(RepoError::runtime(format!("'{}' object has no attribute 'get'", py_type(other)))),
    }
}

fn body_json(resp: ureq::Response, limits: &Limits) -> Result<Value> {
    let bytes = read_all(resp, limits)?;
    let text = String::from_utf8(bytes).map_err(|e| RepoError::runtime(format!("'utf-8' codec can't decode bytes: {e}")))?;
    json::loads(&text).map_err(|e| RepoError::runtime(e.to_string()))
}

impl HttpsRemote {
    pub fn new(url: &str) -> Result<HttpsRemote> {
        if let Some(why) = urllib_proxy(url) {
            return Err(RepoError::forward(format!("urllib would use a proxy ({why})")));
        }
        if let Some(rest) = url.split_once("://").map(|x| x.1) {
            let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
            if netloc.contains('@') {
                return Err(RepoError::forward("credentials in the URL"));
            }
        }
        let limits = Limits {
            reply: env_limit("TMIHUB_MAX_REPLY_BYTES", MAX_REPLY_BYTES).min(1 << 62) as u64,
            download: env_limit("TMIHUB_MAX_DOWNLOAD_BYTES", MAX_DOWNLOAD_BYTES).min(1 << 62) as u64,
            window: Duration::from_secs(env_limit("TMIHUB_PACE_WINDOW", PACE_WINDOW).min(1 << 31) as u64),
            min_bytes: env_limit("TMIHUB_PACE_MIN_BYTES", PACE_MIN_BYTES).min(1 << 62) as u64,
        };
        let seconds = env_limit("TMIHUB_TIMEOUT", TIMEOUT).min(1 << 31) as u64;
        let timeout = Duration::from_secs(seconds);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .redirects(0)
            // urllib's one connection per request (`Connection: close`):
            // no reused connection the server may have closed meanwhile
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .build();
        let url = url.trim_end_matches('/').to_string();
        let origin = url_origin(&url);
        let (token, unbound_token) = match resolve_token().filter(|(t, _)| !t.is_empty()) {
            Some((t, Some(host))) if token_host_matches(&host, &origin) => (Some(t), false),
            Some(_) => (None, true),
            None => (None, false),
        };
        Ok(HttpsRemote { url, token, unbound_token, origin, limits, agent })
    }

    /// A request to `endpoint`. The token goes only to the origin of the
    /// remote URL and never over plain http (except to this machine).
    fn request(&self, method: &str, endpoint: &str) -> Result<ureq::Request> {
        let full = format!("{}/{endpoint}", self.url);
        let mut req = self.agent.request(method, &full);
        if let Some(t) = self.token.as_deref().filter(|t| !t.is_empty()) {
            let target = url_origin(&full);
            if target == self.origin {
                if target.scheme != "https" && !is_loopback(&target.host) {
                    return Err(RepoError::runtime(plain_http_token(&target.netloc)));
                }
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
        }
        Ok(req)
    }

    /// The outcome of a request as `_open` sees it.
    fn classify(&self, result: std::result::Result<ureq::Response, ureq::Error>) -> std::result::Result<ureq::Response, Failure> {
        let first = !TALKED.load(Ordering::SeqCst);
        if !matches!(&result, Err(ureq::Error::Transport(t)) if Self::connecting(t)) {
            TALKED.store(true, Ordering::SeqCst);
        }
        match result {
            Ok(resp) if (200..300).contains(&resp.status()) => Ok(resp),
            // a redirect is never followed (the token must not travel on)
            Ok(resp) if (300..400).contains(&resp.status()) => {
                Err(Failure::Redirect(redirect_text(resp.status(), resp.header("Location"))))
            }
            Ok(resp) => Err(Failure::Other {
                text: format!("HTTP {} without redirect handling", resp.status()),
                url_error: false,
                forward: first,
            }),
            Err(ureq::Error::Transport(t)) => Err(Self::transport_failure(&t, first)),
            Err(ureq::Error::Status(code, resp)) => {
                let mut bytes = Vec::new();
                let _ = Body::new(resp.into_reader(), &self.limits).take(MAX_ERROR_BYTES).read_to_end(&mut bytes);
                let detail = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|t| json::loads(t).ok())
                    .and_then(|v| match v {
                        Value::Object(m) => Some(m.get("error").cloned().unwrap_or(Value::Str(String::new()))),
                        _ => None,
                    })
                    .filter(Value::truthy)
                    .and_then(|v| v.py_str())
                    .unwrap_or_default();
                if matches!(code, 401 | 403) && self.unbound_token {
                    let text = format!("tmihub denied access ({code}); {}", unbound_token_text(&self.origin.netloc));
                    return Err(Failure::Other { text, url_error: true, forward: false });
                }
                Err(Failure::Status(code, detail))
            }
        }
    }

    /// A failure before any byte of the request went out (DNS, connect,
    /// TLS handshake).
    fn connecting(t: &ureq::Transport) -> bool {
        matches!(t.kind(), ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed)
    }

    /// A failure below HTTP, worded as Python's exception: urlopen raises
    /// URLError for the connect and the sending of the request, and the
    /// socket's own exception while it waits for the response.
    fn transport_failure(t: &ureq::Transport, first: bool) -> Failure {
        let timeout = timed_out(t);
        let os = os_error(t);
        let os_text = || match os {
            Some(io) => py_os_text(io),
            None => t.to_string(),
        };
        if Self::connecting(t) {
            let inner = match (timeout, os) {
                (true, _) => "timed out".to_string(),
                // socket.gaierror on Windows
                (false, Some(io)) if cfg!(windows) && t.kind() == ureq::ErrorKind::Dns => {
                    format!("[Errno {}] getaddrinfo failed", io.raw_os_error().unwrap_or(0))
                }
                _ => os_text(),
            };
            return Failure::Other { text: format!("<urlopen error {inner}>"), url_error: true, forward: first && !timeout };
        }
        // a write of the request that failed: the socket's error itself
        // (reading the response wraps it in ureq's context)
        let source = std::error::Error::source(t).and_then(|s| s.downcast_ref::<std::io::Error>());
        if t.kind() == ureq::ErrorKind::Io && source.is_some_and(|io| io.get_ref().is_none()) {
            let inner = if timeout { "timed out".to_string() } else { os_text() };
            return Failure::Other { text: format!("<urlopen error {inner}>"), url_error: true, forward: false };
        }
        let closed = source
            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionAborted && io.to_string() == "Unexpected EOF");
        let text = if closed {
            // http.client.RemoteDisconnected
            "Remote end closed connection without response".to_string()
        } else {
            receive_text(t)
        };
        Failure::Other { text, url_error: false, forward: false }
    }

    /// `_open`'s errors as RepoError: HTTP statuses in Python's words;
    /// others as Python prints them, or Forward for a first request that
    /// could not connect while nothing has been written.
    fn fail(f: Failure) -> RepoError {
        match f {
            Failure::Status(code, detail) => {
                let suffix = if detail.is_empty() { String::new() } else { format!(": {detail}") };
                RepoError::runtime(match code {
                    401 | 403 => format!(
                        "tmihub denied access ({code}){suffix}. set a token with 'tmiki config --global tmihub.token <PAT>' \
                         and the hub it is for with 'tmiki config --global tmihub.host <host>' \
                         (or TMIHUB_TOKEN and TMIHUB_HOST)"
                    ),
                    409 => format!(
                        "tmihub rejected the update ({code}){suffix}. Fetch and merge first (HTTPS remotes do not accept force pushes)."
                    ),
                    _ => format!("tmihub request failed ({code}){suffix}"),
                })
            }
            Failure::Redirect(text) => RepoError::runtime(text),
            Failure::Other { text, forward, .. } => {
                if forward && !tmi_repo::fsio::mutated() {
                    RepoError::forward(format!("network error before the first request: {text}"))
                } else {
                    RepoError::runtime(text)
                }
            }
        }
    }

    fn get_json(&self, endpoint: &str) -> Result<Value> {
        let resp = self.classify(self.request("GET", endpoint)?.call()).map_err(Self::fail)?;
        body_json(resp, &self.limits)
    }

    pub fn list_refs(&self) -> Result<Refs> {
        let data = self.get_json("refs")?;
        let mut out = Refs::new();
        match get_or(&data, "refs")? {
            None => {}
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    out.insert(k.clone(), ref_value(v)?);
                }
            }
            Some(other) if !other.truthy() => {}
            Some(other) => return Err(RepoError::runtime(format!("'{}' object has no attribute 'items'", py_type(other)))),
        }
        Ok(out)
    }

    fn server_oids(&self) -> Result<Vec<String>> {
        let data = self.get_json("objects/list")?;
        match get_or(&data, "oids")? {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    // what is no object id (a path, a share) is not asked for
                    Value::Str(s) => tmi_store::limits::is_oid(s).then(|| Ok(s.clone())),
                    other => Some(Err(RepoError::runtime(format!("'{}' object is not subscriptable", py_type(other))))),
                })
                .collect(),
            Some(other) => Err(RepoError::runtime(format!("'{}' object is not iterable", py_type(other)))),
        }
    }

    pub fn pull_objects(&self, into: &ObjectStore, progress: Progress) -> Result<usize> {
        let local: std::collections::HashSet<String> = into.iter_object_ids().into_iter().collect();
        let missing: Vec<String> = self.server_oids()?.into_iter().filter(|o| !local.contains(o)).collect();
        self.fetch_objects(into, &missing, progress)
    }

    /// `fetch_objects(into_store, oids)`.
    pub fn fetch_objects(&self, into: &ObjectStore, oids: &[String], progress: Progress) -> Result<usize> {
        let mut seen = std::collections::HashSet::new();
        let missing: Vec<String> =
            oids.iter().filter(|o| seen.insert(o.as_str()) && !into.exists(o)).cloned().collect();
        if missing.is_empty() {
            return Ok(0);
        }
        progress(&format!("fetching {} remote objects via HTTP...", missing.len()));
        let batches = batches(&missing, None)?;
        let mut received = 0;
        let mut done = 0;
        for (index, batch) in batches.iter().enumerate() {
            progress(&format!(
                "fetching objects [{done}/{}] (batch {}/{})",
                missing.len(),
                index + 1,
                batches.len()
            ));
            received += self.download_batch(into, batch)?;
            done += batch.len();
        }
        progress(&format!("fetching objects [{done}/{}] done", missing.len()));
        Ok(received)
    }

    fn download_batch(&self, into: &ObjectStore, oids: &[String]) -> Result<usize> {
        let payload = json::dumps(
            &Value::Object(Map::from_iter([(
                "oids".to_string(),
                Value::Array(oids.iter().map(|o| Value::Str(o.clone())).collect()),
            )])),
            &DumpOptions::DEFAULT,
        );
        let req = self.request("POST", "objects/download")?.set("Content-Type", "application/json");
        let mut spool = Spool::new("")?;
        {
            let resp = self.classify(req.send_bytes(payload.as_bytes())).map_err(Self::fail)?;
            let limit = self.limits.download;
            if resp.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()).is_some_and(|n| n > limit) {
                return Err(RepoError::runtime(too_big_download(limit)));
            }
            // shutil.copyfileobj(response, spool): a short body ends early;
            // one past the limit stops (the spool is removed as it drops)
            let mut body = Body::new(resp.into_reader(), &self.limits);
            let mut buf = vec![0u8; 1 << 20];
            let mut total = 0u64;
            loop {
                let n = body.read(&mut buf).map_err(body_error)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > limit {
                    return Err(RepoError::runtime(too_big_download(limit)));
                }
                spool.file.write_all(&buf[..n]).map_err(body_error)?;
            }
        }
        spool.file.rewind().map_err(|e| RepoError::runtime(e.to_string()))?;
        let mut zip = zipio::Reader::open(&spool.file).map_err(|e| RepoError::runtime(e.to_string()))?;
        // The members Python's loop takes, in order: "<2>/<62>" names whose
        // object is not stored yet (a repeated name finds the first one's
        // stored; `zf.open(name)` reads the last member of that name).
        let last: std::collections::HashMap<&str, usize> =
            zip.entries.iter().enumerate().map(|(i, e)| (e.name.as_str(), i)).collect();
        let mut seen = std::collections::HashSet::new();
        let mut wanted: Vec<(usize, String)> = Vec::new();
        for entry in &zip.entries {
            let parts: Vec<&str> = entry.name.split('/').collect();
            if parts.len() != 2 || parts[0].chars().count() != 2 || parts[1].chars().count() != 62 {
                continue;
            }
            let oid = format!("{}{}", parts[0], parts[1]);
            // a name that is no object id is skipped before it reaches a path
            if !tmi_store::limits::is_oid(&oid) || into.exists(&oid) || !seen.insert(oid.clone()) {
                continue;
            }
            wanted.push((last[entry.name.as_str()], oid));
        }
        // Read a group, check it on every core, store it; the first failure
        // (in order) stops there with the ones before it stored, as Python's
        // one-by-one loop leaves it.
        let mut received = 0;
        let mut at = 0;
        while at < wanted.len() {
            let mut end = at;
            let mut bytes = 0u64;
            while end < wanted.len() && (end == at || (end - at < 64 && bytes < 64 << 20)) {
                // inflated sizes count: a group of small deflate bombs is not small
                bytes += zip.entries[wanted[end].0].held_size();
                end += 1;
            }
            let mut group: Vec<(&str, std::result::Result<Vec<u8>, RepoError>)> = Vec::new();
            for (i, oid) in &wanted[at..end] {
                let data = zip.read(*i).map_err(|e| RepoError::runtime(e.to_string()));
                let failed = data.is_err();
                group.push((oid, data));
                if failed {
                    break;
                }
            }
            let checked: Vec<std::result::Result<(), RepoError>> = group
                .par_iter()
                .map(|(oid, data)| match data {
                    Ok(d) => rawobj::verify(oid, d).map_err(|e| {
                        if e.starts_with(rawobj::REFUSED_TREE) {
                            RepoError::runtime(e)
                        } else {
                            RepoError::runtime(format!("remote sent corrupt object data: {e}"))
                        }
                    }),
                    Err(e) => Err(e.clone()),
                })
                .collect();
            let good = checked.iter().position(|r| r.is_err()).unwrap_or(checked.len());
            let stored: Vec<std::result::Result<(), RepoError>> = group[..good]
                .par_iter()
                .map(|(oid, data)| rawobj::store(into, oid, data.as_ref().unwrap()).map_err(RepoError::runtime))
                .collect();
            for r in stored {
                r?;
                received += 1;
            }
            if good < checked.len() {
                return Err(checked.into_iter().nth(good).unwrap().unwrap_err());
            }
            at = end;
        }
        Ok(received)
    }

    /// Send the objects of `wanted` (what the push reaches) the server lacks.
    pub fn push_objects(&self, from: &ObjectStore, wanted: &crate::Wanted, progress: Progress) -> Result<usize> {
        let server: std::collections::HashSet<String> = self.server_oids()?.into_iter().collect();
        let mut missing: Vec<String> = wanted.iter().filter(|o| !server.contains(*o)).cloned().collect();
        missing.sort();
        if missing.is_empty() {
            return Ok(0);
        }
        progress(&format!("pushing {} local objects via HTTP...", missing.len()));
        let size_of = |oid: &str| -> Result<i128> {
            let path = from.path_for(oid);
            std::fs::metadata(&path)
                .map(|m| m.len() as i128)
                .map_err(|e| RepoError::runtime(format!("{}: '{}'", tmi_repo::hooks::os_error_str(&e), path.display())))
        };
        let batches = batches(&missing, Some(&size_of))?;
        let mut done = 0;
        for (index, batch) in batches.iter().enumerate() {
            progress(&format!(
                "pushing objects [{done}/{}] (batch {}/{})",
                missing.len(),
                index + 1,
                batches.len()
            ));
            self.upload_batch(from, batch)?;
            done += batch.len();
        }
        progress(&format!("pushing objects [{done}/{}] done", missing.len()));
        Ok(missing.len())
    }

    fn upload_batch(&self, from: &ObjectStore, oids: &[String]) -> Result<()> {
        let spool = Spool::new(".zip")?;
        let io = |e: std::io::Error| RepoError::runtime(e.to_string());
        let mut writer = zipio::Writer::new(std::io::BufWriter::new(&spool.file));
        for oid in oids {
            let path = from.path_for(oid);
            let data = std::fs::read(&path)
                .map_err(|e| RepoError::runtime(format!("{}: '{}'", tmi_repo::hooks::os_error_str(&e), path.display())))?;
            writer.add(&format!("{}/{}", &oid[..2.min(oid.len())], &oid[2.min(oid.len())..]), &data, false).map_err(io)?;
        }
        writer.finish().map_err(io)?.flush().map_err(io)?;
        let zip_len = spool.file.metadata().map_err(io)?.len();
        let boundary = random_hex(7, 32);
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"objects.zip\"\r\nContent-Type: application/zip\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let length = head.len() as u64 + zip_len + tail.len() as u64;
        let mut file = std::fs::File::open(&spool.path).map_err(io)?;
        file.rewind().map_err(io)?;
        let body = std::io::Cursor::new(head.into_bytes()).chain(file).chain(std::io::Cursor::new(tail.into_bytes()));
        let req = self
            .request("POST", "objects/upload")?
            .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .set("Content-Length", &length.to_string());
        // `with self._open(req): pass`: the body is not read
        self.classify(req.send(body)).map_err(Self::fail)?;
        Ok(())
    }

    pub fn update_ref(&self, name: &str, value: &str, expect: crate::Expect<'_>) -> Result<()> {
        let mut body = Map::from_iter([
            ("ref_name".to_string(), Value::Str(name.to_string())),
            ("value".to_string(), Value::Str(value.to_string())),
        ]);
        if let crate::Expect::Is(want) = expect {
            // compare-and-swap: the server must refuse (409) unless the ref is
            // still `expected` (null: it must not exist) when it applies this
            body.insert("expected".to_string(), want.map_or(Value::Null, |w| Value::Str(w.to_string())));
        }
        let payload = json::dumps(&Value::Object(body), &DumpOptions::DEFAULT);
        let req = self.request("POST", "update_ref")?.set("Content-Type", "application/json");
        // `self._open(req)`: the body is not read
        self.classify(req.send_bytes(payload.as_bytes())).map_err(Self::fail)?;
        Ok(())
    }

    /// `head_branch()`: None when the request fails with an HTTP status or
    /// a URLError (`except (URLError, RuntimeError)`); what urllib raises
    /// while receiving (a timeout, ...) and a bad body still fail.
    pub fn head_branch(&self) -> Result<Option<Option<String>>> {
        let resp = match self.classify(self.request("GET", "head")?.call()) {
            Ok(r) => r,
            Err(Failure::Status(..)) | Err(Failure::Redirect(_)) | Err(Failure::Other { url_error: true, .. }) => return Ok(None),
            Err(other) => return Err(Self::fail(other)),
        };
        let data = body_json(resp, &self.limits)?;
        Ok(match get_or(&data, "head_branch")? {
            None | Some(Value::Null) => None,
            Some(Value::Str(s)) if s.is_empty() => None,
            Some(Value::Str(s)) => Some(Some(s.clone())),
            Some(v) if !v.truthy() => None,
            Some(_) => Some(None),
        })
    }
}

/// A ref value from the server: falsy ones count as empty.
fn ref_value(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        other if !other.truthy() => Ok(String::new()),
        other => Err(RepoError::runtime(format!("can only concatenate str (not \"{}\") to str", py_type(other)))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn objects_in_transit_are_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let spool = super::Spool::new(".zip").unwrap();
        assert_eq!(spool.file.metadata().unwrap().permissions().mode() & 0o077, 0);
    }
}
