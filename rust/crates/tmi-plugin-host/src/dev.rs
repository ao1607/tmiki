//! `tmiki plugin dev` (PLUGINS.md 7.1, 5): a development plugin, an ordinary
//! executable that speaks the plugin interface over its stdin and stdout
//! (`tmi_plugin_dev`'s doc has the wire), as a [`PluginBackend`].
//!
//! [`DevProcessBackend::start`] runs the program, says `hello`, reads and
//! checks `info` (the rules a component's `info` follows). Each call then
//! goes down the pipe; the host functions the plugin calls back
//! (`source.read`, `codecs.transform`, `edit.encode-image`, ...) are answered
//! here by the same functions that answer a wasm plugin (`host.rs`,
//! `tmi_format::write::WriteCx`). Calls are one at a time (a lock: the
//! pipeline's parallel `read-layer`s wait their turn).
//!
//! Nothing a development plugin does can crash tmiki: a call that runs past
//! the time limit (`Settings::timeout`, else 120 s) kills the process
//! (`PluginError::Timeout`); a process that exits, closes its stdout or
//! writes something that is not a frame is a `PluginError::Trap` naming
//! what happened (with the panic message from its stderr when there is
//! one), and the next call starts the program again. The plugin's stderr
//! is shown as `tmiki: plugin <id> (dev): <line>`.
//!
//! What a development plugin is not: sandboxed (it is the user's own
//! program, run because they named it: `TMI_PLUGIN_DEV`), deterministic
//! across calls (one process answers them all), or recordable: the
//! records it makes carry `importer.component_sha256 = "dev"`, which no
//! commit takes (PLUGINS.md 5).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tmi_format::pipeline::Importer;
use tmi_format::types as t;
use tmi_format::write::WriteCx;
use tmi_plugin_dev::{read_frame, write_frame, Message, Value, ValueError, DEV_COMPONENT, PROTOCOL};

use crate::backend::PluginBackend;
use crate::bindings::tmi::format::sqlite;
use crate::devwire as wire;
use crate::host;
use crate::plugin::{panic_message, PluginError};
use crate::reader::BackendReader;

/// The time a call may take when no `plugin.timeout` says otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The handle of a call's source file.
const SOURCE_HANDLE: u64 = 0;

/// The last bytes of the plugin's stderr kept for messages.
const STDERR_TAIL: usize = 64 << 10;

/// A development plugin's command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Where it runs (`None`: tmiki's own working directory). A relative
    /// program path with a directory in it is taken from there too.
    pub dir: Option<std::path::PathBuf>,
}

impl DevCommand {
    pub fn new(program: impl Into<OsString>, args: impl IntoIterator<Item = impl Into<OsString>>) -> DevCommand {
        DevCommand { program: program.into(), args: args.into_iter().map(Into::into).collect(), dir: None }
    }

    /// The same command run in `dir`.
    pub fn in_dir(mut self, dir: impl Into<std::path::PathBuf>) -> DevCommand {
        self.dir = Some(dir.into());
        self
    }

    /// The program to start: in `dir` when it is a relative path with a
    /// directory in it (`target/debug/x`), else as given (`python`: PATH).
    pub fn program_path(&self) -> OsString {
        let p = std::path::Path::new(&self.program);
        match &self.dir {
            Some(d) if p.is_relative() && p.components().count() > 1 => d.join(p).into_os_string(),
            _ => self.program.clone(),
        }
    }

    /// The command line, for messages.
    pub fn display(&self) -> String {
        std::iter::once(&self.program).chain(&self.args).map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }
}

/// How a development plugin is run.
#[derive(Clone, Debug)]
pub struct DevOptions {
    /// Per call; `None`: [`DEFAULT_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// `Settings::sqlite_reverse_unordered_selects` (`tmiki plugin test`).
    pub sqlite_reverse_unordered_selects: bool,
}

impl Default for DevOptions {
    fn default() -> Self {
        DevOptions { timeout: Some(DEFAULT_TIMEOUT), sqlite_reverse_unordered_selects: false }
    }
}

impl DevOptions {
    fn limit(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
}

/// A call that did not answer: whether the process can answer the next one.
enum Fail {
    /// The plugin's own failure (a trap); the process is fine.
    Call(PluginError),
    /// The process is gone or out of step: it is stopped and started again
    /// on the next call.
    Process(PluginError),
}

impl Fail {
    fn error(self) -> PluginError {
        match self {
            Fail::Call(e) | Fail::Process(e) => e,
        }
    }
}

type Frame = Result<Value, String>;

/// One running copy of the program.
struct Process {
    child: Child,
    /// Frames for the thread that writes the plugin's stdin (a write into a
    /// pipe the plugin does not read blocks: only that thread may block, and
    /// [`Process::send`] waits for it with the call's deadline).
    to_stdin: Option<mpsc::Sender<Vec<u8>>>,
    /// The writer thread's answer to each frame.
    written: Receiver<std::io::Result<()>>,
    frames: Receiver<Frame>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

impl Process {
    fn spawn(cmd: &DevCommand, label: Arc<OnceLock<String>>) -> Result<Process, PluginError> {
        let mut command = Command::new(cmd.program_path());
        if let Some(d) = &cmd.dir {
            command.current_dir(d);
        }
        let mut child = command
            .args(&cmd.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PluginError::Compile(format!("cannot run the development plugin {}: {e}", cmd.display())))?;
        let mut stdin = child.stdin.take().expect("piped");
        let (to_stdin, jobs) = mpsc::channel::<Vec<u8>>();
        let (done, written) = mpsc::channel::<std::io::Result<()>>();
        std::thread::Builder::new()
            .name("tmi-dev-plugin-stdin".into())
            .spawn(move || {
                // ends when the sender is dropped, or the pipe breaks (the process is killed)
                while let Ok(frame) = jobs.recv() {
                    let r = stdin.write_all(&frame).and_then(|()| stdin.flush());
                    let failed = r.is_err();
                    if done.send(r).is_err() || failed {
                        return;
                    }
                }
            })
            .map_err(|e| PluginError::Compile(format!("a thread for the development plugin: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut err_pipe = child.stderr.take().expect("piped");
        let (tx, frames) = mpsc::channel::<Frame>();
        std::thread::Builder::new()
            .name("tmi-dev-plugin-stdout".into())
            .spawn(move || loop {
                match read_frame(&mut stdout) {
                    Ok(Some(v)) => {
                        if tx.send(Ok(v)).is_err() {
                            return;
                        }
                    }
                    // the end: the receiver sees the channel close
                    Ok(None) => return,
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                }
            })
            .map_err(|e| PluginError::Compile(format!("a thread for the development plugin: {e}")))?;
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr.clone();
        std::thread::Builder::new()
            .name("tmi-dev-plugin-stderr".into())
            .spawn(move || {
                let mut line = Vec::new();
                let mut buf = [0u8; 8192];
                let show = |line: &[u8]| {
                    let who = label.get().map(String::as_str).unwrap_or("dev");
                    eprintln!("tmiki: plugin {who} (dev): {}", tmi_format::safe_text::message(&String::from_utf8_lossy(line)));
                };
                loop {
                    let n = match err_pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    {
                        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                        t.extend(&buf[..n]);
                        while t.len() > STDERR_TAIL {
                            t.pop_front();
                        }
                    }
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            show(&line);
                            line.clear();
                        } else {
                            line.push(b);
                        }
                    }
                }
                if !line.is_empty() {
                    show(&line);
                }
            })
            .map_err(|e| PluginError::Compile(format!("a thread for the development plugin: {e}")))?;
        Ok(Process { child, to_stdin: Some(to_stdin), written, frames, stderr })
    }

    fn stderr_text(&self) -> String {
        let t = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let (a, b) = t.as_slices();
        let mut v = a.to_vec();
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }

    /// Why the process stopped answering: its exit status (waited for a
    /// moment) and the panic it printed, if any.
    fn gone(&mut self, what: &str) -> PluginError {
        let deadline = Instant::now() + Duration::from_millis(2000);
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => break None,
            }
        };
        // let the stderr thread take the last lines
        std::thread::sleep(Duration::from_millis(50));
        let status = match status {
            Some(s) => match s.code() {
                Some(c) => format!("; it exited with code {c}"),
                None => format!("; it exited ({s})"),
            },
            None => String::new(),
        };
        let panic = panic_message(&self.stderr_text()).map(|m| format!(" (panic: {m})")).unwrap_or_default();
        PluginError::Trap(format!("the development plugin {what}{status}{panic}"))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Sends `m` and waits until the plugin has taken it, or `deadline`
    /// (a plugin that asked for a large answer and does not read its stdin
    /// is a timeout, not a hang of tmiki).
    fn send(&mut self, m: Message, deadline: Instant, limit: Duration) -> Result<(), Fail> {
        let mut frame = Vec::new();
        write_frame(&mut frame, &m.into_value()).map_err(|e| Fail::Process(PluginError::Trap(format!("a message to the development plugin: {e}"))))?;
        let sent = self.to_stdin.as_ref().is_some_and(|tx| tx.send(frame).is_ok());
        if !sent {
            return Err(Fail::Process(self.gone("closed its stdin")));
        }
        match self.written.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => Err(Fail::Process(self.gone("closed its stdin"))),
            Err(RecvTimeoutError::Timeout) => Err(Fail::Process(PluginError::Timeout { limit })),
        }
    }

    fn recv(&mut self, deadline: Instant, limit: Duration) -> Result<Message, Fail> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.frames.recv_timeout(left) {
            Ok(Ok(v)) => Message::from_value(v).map_err(|e| Fail::Process(PluginError::Trap(format!("the development plugin sent a message tmiki does not read: {e}")))),
            Ok(Err(e)) => Err(Fail::Process(PluginError::Trap(format!("the development plugin wrote {e}")))),
            Err(RecvTimeoutError::Timeout) => Err(Fail::Process(PluginError::Timeout { limit })),
            Err(RecvTimeoutError::Disconnected) => Err(Fail::Process(self.gone("stopped answering (its stdout closed)"))),
        }
    }

    /// One call: the message, the imports it makes, its answer.
    fn converse(&mut self, func: &str, args: Vec<Value>, cx: &mut CallCx, limit: Duration) -> Result<Value, Fail> {
        let deadline = Instant::now() + limit;
        self.send(Message::Call { func: func.to_string(), args }, deadline, limit)?;
        // a host function that trapped ends the call; the plugin's answer
        // (its own trap, normally) is read to stay in step
        let mut trapped: Option<String> = None;
        loop {
            let message = match (self.recv(deadline, limit), &trapped) {
                // a plugin built with panic=abort ends at the host's trap:
                // the trap is the reason
                (Err(Fail::Process(PluginError::Trap(_))), Some(m)) => return Err(Fail::Process(PluginError::Trap(m.clone()))),
                (r, _) => r?,
            };
            match message {
                Message::Import { func, args } => match cx.import(&func, &args) {
                    Ok(v) if trapped.is_none() => self.send(Message::Return(v), deadline, limit)?,
                    Ok(_) => self.send(Message::Trap(trapped.clone().unwrap_or_default()), deadline, limit)?,
                    Err(m) => {
                        let m = format!("{func}: {m}");
                        self.send(Message::Trap(m.clone()), deadline, limit)?;
                        trapped.get_or_insert(m);
                    }
                },
                Message::Return(v) => {
                    return match trapped {
                        Some(m) => Err(Fail::Call(PluginError::Trap(m))),
                        None => Ok(v),
                    }
                }
                Message::Trap(m) => return Err(Fail::Call(PluginError::Trap(trapped.unwrap_or(m)))),
                Message::Call { func, .. } => {
                    return Err(Fail::Process(PluginError::Trap(format!("the development plugin sent a call ({func}) where it should answer one"))))
                }
            }
        }
    }
}

/// What a call's host functions see: its file and, for a write, the
/// `edit` resources.
struct CallCx<'a> {
    id: &'a str,
    source: Option<&'a [u8]>,
    name: &'a str,
    write: Option<&'a mut WriteCx>,
    databases: Vec<Option<rusqlite::Connection>>,
    reverse: bool,
}

fn arg<'v>(args: &'v [Value], i: usize) -> Result<&'v Value, String> {
    args.get(i).ok_or_else(|| format!("argument {} is missing", i + 1))
}

fn ve(e: ValueError) -> String {
    e.0
}

fn result_value(r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => Value::ok(v),
        Err(e) => Value::err(Value::Text(e)),
    }
}

impl CallCx<'_> {
    fn source_handle(&self, v: &Value) -> Result<&[u8], String> {
        let h = v.u64("the source-file handle").map_err(ve)?;
        match self.source {
            Some(s) if h == SOURCE_HANDLE => Ok(s),
            Some(_) => Err(format!("{h} is not a source-file handle of this call")),
            None => Err("this call has no source file".into()),
        }
    }

    fn write_cx(&mut self) -> Result<&mut WriteCx, String> {
        self.write.as_deref_mut().ok_or_else(|| "edit is only for write-layers".to_string())
    }

    fn handle(v: &Value, what: &str) -> Result<u32, String> {
        v.u32(what).map_err(ve)
    }

    fn database(&self, v: &Value) -> Result<&rusqlite::Connection, String> {
        let h = v.u64("the database handle").map_err(ve)?;
        self.databases.get(h as usize).and_then(Option::as_ref).ok_or_else(|| format!("{h} is not an open database"))
    }

    fn params(v: &Value) -> Result<Vec<sqlite::SqlValue>, String> {
        v.array("params").map_err(ve)?.iter().map(|p| wire::sql_value(p, "params[]").map_err(ve)).collect()
    }

    /// Host function `func`. `Err`: a trap (the message names what is
    /// wrong); the WIT's own `result` errors are `Ok(Value::err(..))`.
    fn import(&mut self, func: &str, args: &[Value]) -> Result<Value, String> {
        let a = |i| arg(args, i);
        Ok(match func {
            "source.source-file.size" => Value::Uint(self.source_handle(a(0)?)?.len() as u64),
            "source.source-file.read" => {
                let data = self.source_handle(a(0)?)?;
                let offset = a(1)?.u64("offset").map_err(ve)?;
                let len = a(2)?.u32("len").map_err(ve)?;
                let start = offset.min(data.len() as u64) as usize;
                let end = start.saturating_add(len as usize).min(data.len());
                Value::Bytes(data[start..end].to_vec())
            }
            "source.source-file.name" => {
                self.source_handle(a(0)?)?;
                Value::text(self.name)
            }
            "source.source-file.drop" => {
                self.source_handle(a(0)?)?;
                Value::Null
            }
            "codecs.transform" => {
                let data = wire::bytes(a(0)?, "data").map_err(ve)?;
                let list = wire::codecs(a(1)?, "codecs").map_err(ve)?;
                let max = a(2)?.u32("max-out").map_err(ve)?;
                result_value(host::transform(self.source, &data, &list, max).map(Value::Bytes))
            }
            "codecs.crc32" => Value::uint(host::crc32(self.source, &wire::bytes(a(0)?, "data").map_err(ve)?)?),
            "codecs.sha256" => Value::Bytes(host::sha256(self.source, &wire::bytes(a(0)?, "data").map_err(ve)?)?),
            "codecs.image-info" => {
                let img = wire::encoded_image(a(0)?, "img").map_err(ve)?;
                result_value(host::image_info(Some(self.source.unwrap_or(&[])), &img, tmi_format::recipe::transform::MAX_EXPANDED).map(|h| wire::image_header_value(&h)))
            }
            "sqlite.database.open" | "sqlite.database.open-writable" => {
                let image = wire::bytes(a(0)?, "image").map_err(ve)?;
                let opened = host::resolve(self.source, &image).and_then(|b| host::open_database(&b, func == "sqlite.database.open", self.reverse));
                result_value(opened.map(|conn| {
                    self.databases.push(Some(conn));
                    Value::Uint(self.databases.len() as u64 - 1)
                }))
            }
            "sqlite.database.query" => {
                let conn = self.database(a(0)?)?;
                let sql = a(1)?.str("sql").map_err(ve)?;
                let rows = host::run_query(conn, sql, Self::params(a(2)?)?);
                result_value(rows.map(|rows| Value::list(&rows, |r| Value::list(r, wire::sql_value_value))))
            }
            "sqlite.database.execute" => {
                let conn = self.database(a(0)?)?;
                let sql = a(1)?.str("sql").map_err(ve)?;
                result_value(host::execute(conn, sql, Self::params(a(2)?)?).map(Value::Uint))
            }
            "sqlite.database.serialize" => result_value(host::serialize(self.database(a(0)?)?).map(Value::Bytes)),
            "sqlite.database.drop" => {
                self.database(a(0)?)?;
                let h = a(0)?.u64("handle").map_err(ve)? as usize;
                self.databases[h] = None;
                Value::Null
            }
            "xml.parse" => {
                let text = a(0)?.byte_slice("text").map_err(ve)?;
                result_value(host::parse_xml(text).map(|els| Value::list(&els, wire::element_value)))
            }
            "log.emit" => {
                let level = a(0)?.enum_name("level").map_err(ve)?;
                if level != "debug" && level != "info" {
                    return Err(format!("level has no case {level:?}"));
                }
                let message = a(1)?.str("message").map_err(ve)?;
                if std::env::var_os("TMI_PLUGIN_LOG").is_some_and(|v| !v.is_empty() && v != "0") {
                    eprintln!("tmiki: plugin {}: {level}: {}", self.id, tmi_format::safe_text::message(message));
                }
                Value::Null
            }
            "edit.pixels.width" => {
                let h = Self::handle(a(0)?, "pixels")?;
                Value::uint(self.write_cx()?.pixels(h)?.width)
            }
            "edit.pixels.height" => {
                let h = Self::handle(a(0)?, "pixels")?;
                Value::uint(self.write_cx()?.pixels(h)?.height)
            }
            "edit.pixels.bbox" => {
                let h = Self::handle(a(0)?, "pixels")?;
                let b = self.write_cx()?.bbox(h)?;
                Value::option(b.map(|(x0, y0, x1, y1)| Value::Array(vec![Value::uint(x0), Value::uint(y0), Value::uint(x1), Value::uint(y1)])))
            }
            "edit.pixels.read" => {
                let h = Self::handle(a(0)?, "pixels")?;
                let n = |i: usize, what: &str| a(i).and_then(|v| v.u32(what).map_err(ve));
                let (x, y, w, hh) = (n(1, "x")?, n(2, "y")?, n(3, "w")?, n(4, "h")?);
                let layout = wire::layout(a(5)?, "layout").map_err(ve)?;
                Value::Bytes(self.write_cx()?.read(h, x, y, w, hh, &layout)?)
            }
            "edit.pixels.drop" | "edit.blob.drop" => {
                Self::handle(a(0)?, "handle")?;
                self.write_cx()?;
                Value::Null
            }
            "edit.blob.len" => {
                let h = Self::handle(a(0)?, "blob")?;
                Value::Uint(self.write_cx()?.blob(h)?.0)
            }
            "edit.blob.crc32" => {
                let h = Self::handle(a(0)?, "blob")?;
                Value::uint(self.write_cx()?.blob(h)?.1)
            }
            "edit.encode-tile" => {
                let h = Self::handle(a(0)?, "px")?;
                let n = |i: usize, what: &str| a(i).and_then(|v| v.u32(what).map_err(ve));
                let (x, y, w, hh) = (n(1, "x")?, n(2, "y")?, n(3, "w")?, n(4, "h")?);
                let layout = wire::layout(a(5)?, "layout").map_err(ve)?;
                let steps: Vec<t::Encoding> = a(6)?.array("steps").map_err(ve)?.iter().map(|s| wire::encoding(s, "steps[]").map_err(ve)).collect::<Result<_, _>>()?;
                let cx = self.write_cx()?;
                // a pixels handle the host never made is the plugin's fault: a trap
                cx.pixels(h)?;
                result_value(cx.encode_tile(h, x, y, w, hh, &layout, &steps).map(Value::uint))
            }
            "edit.encode-image" => {
                let h = Self::handle(a(0)?, "px")?;
                let enc = wire::image_encoding(a(1)?, "enc").map_err(ve)?;
                let cx = self.write_cx()?;
                cx.pixels(h)?;
                result_value(cx.encode_image(h, &enc).map(Value::uint))
            }
            "edit.composite-after-edit" => result_value(self.write_cx()?.composite_after_edit().map(Value::uint)),
            "edit.thumbnail" => {
                let h = Self::handle(a(0)?, "px")?;
                let max = a(1)?.u32("max-side").map_err(ve)?;
                Value::uint(self.write_cx()?.thumbnail(h, max)?)
            }
            other => return Err(format!("tmiki has no host function {other:?}")),
        })
    }
}

/// A development plugin.
pub struct DevProcessBackend {
    cmd: DevCommand,
    opts: DevOptions,
    info: t::PluginInfo,
    abi: String,
    writer: bool,
    label: Arc<OnceLock<String>>,
    process: Mutex<Option<Process>>,
}

impl DevProcessBackend {
    /// Runs `cmd`, says `hello` and reads its `info` (checked as a
    /// component's is). The process stays to answer the calls.
    pub fn start(cmd: DevCommand, opts: DevOptions) -> Result<DevProcessBackend, PluginError> {
        let label = Arc::new(OnceLock::new());
        let mut process = Process::spawn(&cmd, label.clone())?;
        let limit = opts.limit();
        let started = (|| -> Result<(String, bool, t::PluginInfo), PluginError> {
            let (abi, writer) = hello(&mut process, limit)?;
            let mut cx = CallCx { id: "dev", source: None, name: "", write: None, databases: Vec::new(), reverse: false };
            let v = process.converse("reader.info", vec![], &mut cx, limit).map_err(Fail::error)?;
            let info = wire::plugin_info(&v).map_err(|e| PluginError::Info(format!("the development plugin's info does not fit the WIT: {e}")))?;
            crate::plugin::check_info(&info)?;
            Ok((abi, writer, info))
        })();
        let (abi, writer, info) = match started {
            Ok(s) => s,
            Err(e) => {
                process.kill();
                return Err(e);
            }
        };
        let _ = label.set(info.id.clone());
        Ok(DevProcessBackend { cmd, opts, info, abi, writer, label, process: Mutex::new(Some(process)) })
    }

    pub fn command(&self) -> &DevCommand {
        &self.cmd
    }

    /// The interface the plugin says it speaks (`tmi:format@0.1.0`).
    pub fn abi(&self) -> &str {
        &self.abi
    }

    /// Whether it exports the `writer` (`can_write` also needs `can-write`).
    pub fn has_writer(&self) -> bool {
        self.writer
    }

    /// Stops the process (the next call starts it again).
    pub fn stop(&self) {
        if let Some(mut p) = self.process.lock().unwrap_or_else(|e| e.into_inner()).take() {
            p.kill();
        }
    }

    /// One call down the pipe, with `cx`'s host functions.
    fn call(&self, func: &str, args: Vec<Value>, cx: &mut CallCx) -> Result<Value, PluginError> {
        let mut guard = self.process.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let mut p = Process::spawn(&self.cmd, self.label.clone())?;
            if let Err(e) = hello(&mut p, self.opts.limit()) {
                p.kill();
                return Err(e);
            }
            *guard = Some(p);
        }
        let p = guard.as_mut().expect("started above");
        match p.converse(func, args, cx, self.opts.limit()) {
            Ok(v) => Ok(v),
            Err(Fail::Call(e)) => Err(e),
            Err(Fail::Process(e)) => {
                p.kill();
                *guard = None;
                Err(e)
            }
        }
    }

    fn cx<'a>(&'a self, source: Option<(&'a str, &'a [u8])>, write: Option<&'a mut WriteCx>) -> CallCx<'a> {
        CallCx { id: &self.info.id, source: source.map(|s| s.1), name: source.map_or("", |s| s.0), write, databases: Vec::new(), reverse: self.opts.sqlite_reverse_unordered_selects }
    }

    /// A value the plugin answered with that does not fit the WIT: its trap.
    fn misfit(&self, func: &str, e: ValueError) -> PluginError {
        PluginError::Trap(format!("its answer to {func} does not fit the WIT: {e}"))
    }

    fn read_result<T>(&self, func: &str, v: Value, ok: impl FnOnce(&Value) -> Result<T, ValueError>) -> Result<T, PluginError> {
        match v.result(func).map_err(|e| self.misfit(func, e))? {
            Ok(x) => ok(x).map_err(|e| self.misfit(func, e)),
            Err(e) => Err(PluginError::Read(wire::read_error(e).map_err(|e| self.misfit(func, e))?)),
        }
    }
}

impl Drop for DevProcessBackend {
    fn drop(&mut self) {
        // closing stdin ends a well-behaved plugin; a stuck one is killed
        if let Some(mut p) = self.process.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            drop(p.to_stdin.take());
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = p.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}

/// `hello`: the protocol version and the plugin's interface and exports.
fn hello(p: &mut Process, limit: Duration) -> Result<(String, bool), PluginError> {
    let mut cx = CallCx { id: "dev", source: None, name: "", write: None, databases: Vec::new(), reverse: false };
    let v = p.converse("hello", vec![Value::Uint(PROTOCOL)], &mut cx, limit).map_err(Fail::error)?;
    let bad = |e: ValueError| PluginError::Link(format!("the development plugin's hello: {e}"));
    let protocol = v.field("hello", "protocol").and_then(|p| p.u64("protocol")).map_err(bad)?;
    if protocol != PROTOCOL {
        return Err(PluginError::Link(format!("the development plugin speaks protocol {protocol}; this tmiki speaks {PROTOCOL}")));
    }
    let abi = v.field("hello", "abi").and_then(|a| a.string("abi")).map_err(bad)?;
    let writer = v.field("hello", "writer").and_then(|w| w.bool("writer")).map_err(bad)?;
    Ok((abi, writer))
}

impl PluginBackend for DevProcessBackend {
    fn info(&self) -> &t::PluginInfo {
        &self.info
    }

    fn importer(&self) -> Importer {
        Importer { id: self.info.id.clone(), version: self.info.version.clone(), component_sha256: DEV_COMPONENT.into(), abi: self.abi.clone() }
    }

    fn can_write(&self) -> bool {
        self.writer && self.info.can_write
    }

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Result<t::Confidence, PluginError> {
        let v = self.call("reader.probe", vec![Value::text(name), Value::bytes(head), Value::Uint(size)], &mut self.cx(None, None))?;
        wire::confidence(&v, "confidence").map_err(|e| self.misfit("reader.probe", e))
    }

    fn read_structure(&self, name: &str, data: &[u8]) -> Result<t::Structure, PluginError> {
        let v = self.call("reader.read-structure", vec![Value::Uint(SOURCE_HANDLE)], &mut self.cx(Some((name, data)), None))?;
        self.read_result("reader.read-structure", v, wire::structure)
    }

    fn read_layer(&self, name: &str, data: &[u8], context: &[u8], token: u32) -> Result<t::Bitmap, PluginError> {
        let args = vec![Value::Uint(SOURCE_HANDLE), Value::bytes(context), Value::uint(token)];
        let v = self.call("reader.read-layer", args, &mut self.cx(Some((name, data)), None))?;
        self.read_result("reader.read-layer", v, |b| wire::bitmap(b, "bitmap"))
    }

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, PluginError> {
        let args = vec![Value::uint(codec_id), Value::bytes(input), Value::uint(out_len)];
        let v = self.call("reader.decode-chunk", args, &mut self.cx(None, None))?;
        self.read_result("reader.decode-chunk", v, |b| b.byte_vec("chunk"))
    }

    fn write_layers(&self, name: &str, data: &[u8], edits: &[t::LayerEdit], mut cx: WriteCx) -> (Result<t::WritePlan, PluginError>, WriteCx) {
        if !self.writer {
            return (Err(PluginError::Write(t::WriteError::Unsupported(format!("plugin {} has no writer", self.info.id)))), cx);
        }
        let given = cx.pixel_count() as u32;
        let args = vec![
            Value::Uint(SOURCE_HANDLE),
            Value::list(edits, wire::layer_edit_value),
            Value::Array((0..given).map(Value::uint).collect()),
        ];
        let r = self.call("writer.write-layers", args, &mut self.cx(Some((name, data)), Some(&mut cx)));
        let func = "writer.write-layers";
        let r = r.and_then(|v| match v.result(func).map_err(|e| self.misfit(func, e))? {
            Ok(plan) => wire::write_plan(plan).map_err(|e| self.misfit(func, e)),
            Err(e) => Err(PluginError::Write(wire::write_error(e).map_err(|e| self.misfit(func, e))?)),
        });
        (r, cx)
    }
}

/// [`DevProcessBackend::start`] as a `FormatReader` / `FormatWriter`
/// (`tmi_format::pipeline::import_plugin(&reader, &reader.backend().importer(), ...)`).
pub fn open(cmd: DevCommand, opts: DevOptions) -> Result<BackendReader<DevProcessBackend>, PluginError> {
    Ok(BackendReader::from_backend(Arc::new(DevProcessBackend::start(cmd, opts)?)))
}

/// `TMI_PLUGIN_DEV`: a program path, or a JSON array of strings (a program
/// and its arguments), or a JSON array of such arrays (several plugins).
pub fn parse_env(value: &str) -> Result<Vec<DevCommand>, String> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(Vec::new());
    }
    if !v.starts_with('[') {
        return Ok(vec![DevCommand::new(v, Vec::<OsString>::new())]);
    }
    let parsed = parse_json_strings(v)?;
    let commands: Vec<Vec<String>> = match parsed {
        JsonList::Strings(s) => vec![s],
        JsonList::Lists(l) => l,
    };
    commands
        .into_iter()
        .map(|mut words| {
            if words.is_empty() {
                return Err("an empty command".to_string());
            }
            let program = words.remove(0);
            Ok(DevCommand::new(program, words))
        })
        .collect()
}

/// One command line as a user writes it (`tmiki plugin test --native`,
/// `tmi-plugin.toml`'s `dev.command`): a JSON array of strings, or words
/// split at spaces (a word in double quotes may hold spaces; no other
/// escapes).
pub fn parse_command(text: &str) -> Result<DevCommand, String> {
    let t = text.trim();
    if t.starts_with('[') {
        return match parse_json_strings(t)? {
            JsonList::Strings(mut w) if !w.is_empty() => {
                let program = w.remove(0);
                Ok(DevCommand::new(program, w))
            }
            _ => Err(format!("{text:?}: one command, a JSON array of strings")),
        };
    }
    let mut words: Vec<String> = Vec::new();
    let (mut cur, mut quoted, mut any) = (String::new(), false, false);
    for ch in t.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if quoted {
        return Err(format!("{text:?}: a double quote is not closed"));
    }
    if any {
        words.push(cur);
    }
    if words.is_empty() {
        return Err("an empty command".into());
    }
    let program = words.remove(0);
    Ok(DevCommand::new(program, words))
}

/// The `TMI_PLUGIN_DEV` value of `cmds` (what `parse_env` reads back).
pub fn env_value(cmds: &[DevCommand]) -> String {
    let one = |c: &DevCommand| -> String {
        let words: Vec<String> = std::iter::once(&c.program).chain(&c.args).map(|a| json_string(&a.to_string_lossy())).collect();
        format!("[{}]", words.join(","))
    };
    match cmds {
        [c] => one(c),
        _ => format!("[{}]", cmds.iter().map(one).collect::<Vec<_>>().join(",")),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

enum JsonList {
    Strings(Vec<String>),
    Lists(Vec<Vec<String>>),
}

/// A JSON array of strings, or of arrays of strings (nothing else).
fn parse_json_strings(text: &str) -> Result<JsonList, String> {
    let mut p = Json { s: text.as_bytes(), i: 0 };
    let bad = || "TMI_PLUGIN_DEV is a program, a JSON array of strings, or a JSON array of such arrays".to_string();
    p.ws();
    p.eat(b'[').ok_or_else(bad)?;
    p.ws();
    let mut strings = Vec::new();
    let mut lists = Vec::new();
    if p.peek() == Some(b']') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            match p.peek() {
                Some(b'"') if lists.is_empty() => strings.push(p.string().ok_or_else(bad)?),
                Some(b'[') if strings.is_empty() => {
                    p.i += 1;
                    let mut words = Vec::new();
                    p.ws();
                    if p.peek() == Some(b']') {
                        p.i += 1;
                    } else {
                        loop {
                            p.ws();
                            words.push(p.string().ok_or_else(bad)?);
                            p.ws();
                            match p.next() {
                                Some(b',') => continue,
                                Some(b']') => break,
                                _ => return Err(bad()),
                            }
                        }
                    }
                    lists.push(words);
                }
                _ => return Err(bad()),
            }
            p.ws();
            match p.next() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(bad()),
            }
        }
    }
    p.ws();
    if p.i != p.s.len() {
        return Err(bad());
    }
    Ok(if lists.is_empty() { JsonList::Strings(strings) } else { JsonList::Lists(lists) })
}

struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        (self.next()? == c).then_some(())
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out: Vec<u16> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match self.next()? {
                b'"' => break,
                b'\\' => {
                    let flush = |bytes: &mut Vec<u8>, out: &mut Vec<u16>| {
                        out.extend(String::from_utf8_lossy(bytes).encode_utf16());
                        bytes.clear();
                    };
                    flush(&mut bytes, &mut out);
                    match self.next()? {
                        b'"' => out.push(b'"' as u16),
                        b'\\' => out.push(b'\\' as u16),
                        b'/' => out.push(b'/' as u16),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(10),
                        b'r' => out.push(13),
                        b't' => out.push(9),
                        b'u' => {
                            let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
                            out.push(u16::from_str_radix(h, 16).ok()?);
                            self.i += 4;
                        }
                        _ => return None,
                    }
                }
                c => bytes.push(c),
            }
        }
        out.extend(String::from_utf8_lossy(&bytes).encode_utf16());
        Some(String::from_utf16_lossy(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_value_names_one_or_more_commands() {
        assert_eq!(parse_env("  ").unwrap(), vec![]);
        assert_eq!(parse_env(r"C:\dev\ora-dev.exe").unwrap(), vec![DevCommand::new(r"C:\dev\ora-dev.exe", Vec::<String>::new())]);
        assert_eq!(parse_env(r#"["cargo", "run", "--features", "dev"]"#).unwrap(), vec![DevCommand::new("cargo", ["run", "--features", "dev"])]);
        assert_eq!(
            parse_env(r#"[["a"], ["b", "x y", "\u00e9\\"]]"#).unwrap(),
            vec![DevCommand::new("a", Vec::<String>::new()), DevCommand::new("b", ["x y", "é\\"])]
        );
        for bad in [r#"[1]"#, r#"["a", ["b"]]"#, r#"[[]]"#, r#"["a""#, r#"["a"] x"#] {
            assert!(parse_env(bad).is_err(), "{bad}");
        }
        let cmds = vec![DevCommand::new("my dev", ["-c", "say(\"hi\")"]), DevCommand::new(r"C:\x", Vec::<String>::new())];
        assert_eq!(parse_env(&env_value(&cmds)).unwrap(), cmds);
        assert_eq!(parse_env(&env_value(&cmds[..1])).unwrap(), cmds[..1]);
    }

    #[test]
    fn a_command_line_is_words_or_a_json_array() {
        assert_eq!(parse_command("target/debug/x-dev").unwrap(), DevCommand::new("target/debug/x-dev", Vec::<String>::new()));
        assert_eq!(
            parse_command(r#"  cargo run --features dev --manifest-path "my src/Cargo.toml" "#).unwrap(),
            DevCommand::new("cargo", ["run", "--features", "dev", "--manifest-path", "my src/Cargo.toml"])
        );
        assert_eq!(parse_command(r#"["C:\\a b\\x.exe", "-v"]"#).unwrap(), DevCommand::new(r"C:\a b\x.exe", ["-v"]));
        assert_eq!(parse_command(r#"x """#).unwrap(), DevCommand::new("x", [""]));
        for bad in ["", "  ", r#"x "y"#, "[]", r#"[["a"]]"#] {
            assert!(parse_command(bad).is_err(), "{bad}");
        }
        let c = DevCommand::new("target/debug/x", Vec::<String>::new()).in_dir("/p");
        assert_eq!(std::path::PathBuf::from(c.program_path()), std::path::Path::new("/p").join("target/debug/x"));
        assert_eq!(DevCommand::new("cargo", Vec::<String>::new()).in_dir("/p").program_path(), OsString::from("cargo"));
    }
}
