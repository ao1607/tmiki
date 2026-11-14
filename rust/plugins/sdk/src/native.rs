//! A development plugin's main loop (`tmiki plugin dev`; the wire is crate
//! `tmi-plugin-dev`'s): tmiki's calls in from stdin, the answers out on
//! stdout, and the host functions the plugin calls in between.

use std::io::{BufReader, BufWriter, Stdin, Stdout, Write};
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;

use tmi_plugin_dev::{read_frame, write_frame, Message, Value, ValueError, PROTOCOL};

use crate::wire;
use crate::*;

struct Pipe {
    input: BufReader<Stdin>,
    output: BufWriter<Stdout>,
}

static PIPE: Mutex<Option<Pipe>> = Mutex::new(None);

/// A host function that trapped (tmiki's message): unwinds the call.
struct HostTrap(String);

fn send(pipe: &mut Pipe, m: Message) {
    if write_frame(&mut pipe.output, &m.into_value()).is_err() {
        // tmiki is gone
        std::process::exit(0);
    }
}

fn receive(pipe: &mut Pipe) -> Message {
    match read_frame(&mut pipe.input) {
        Ok(Some(v)) => match Message::from_value(v) {
            Ok(m) => m,
            Err(e) => fatal(&format!("a message from tmiki that this SDK does not read: {e}")),
        },
        Ok(None) => std::process::exit(0),
        Err(e) => fatal(&format!("tmiki sent {e}")),
    }
}

fn fatal(m: &str) -> ! {
    eprintln!("tmi-plugin-sdk: {m}");
    std::process::exit(2)
}

/// Calls host function `func` (during a call) and waits for its answer.
pub(crate) fn import(func: &str, args: Vec<Value>) -> Value {
    let answer = {
        let mut guard = PIPE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pipe) = guard.as_mut() else {
            panic!("{func}: the host is only there while tmiki calls the plugin (tmi_plugin_sdk::dev_main)");
        };
        send(pipe, Message::Import { func: func.to_string(), args });
        receive(pipe)
    };
    match answer {
        Message::Return(v) => v,
        Message::Trap(m) => panic::panic_any(HostTrap(m)),
        Message::Call { func: f, .. } | Message::Import { func: f, .. } => fatal(&format!("tmiki sent {f} where it should answer {func}")),
    }
}

/// tmiki answered with something that does not fit the WIT.
pub(crate) fn misfit(func: &str, e: ValueError) -> ! {
    panic!("tmiki's answer to {func} does not fit the WIT: {e}")
}

fn arg<'a>(args: &'a [Value], i: usize, func: &str) -> Result<&'a Value, String> {
    args.get(i).ok_or_else(|| format!("{func}: argument {} is missing", i + 1))
}

fn source(handle: &Value) -> Result<SourceFile<'static>, String> {
    let handle = handle.u64("src").map_err(|e| e.0)?;
    Ok(SourceFile(crate::sys::Source { handle, _call: PhantomData }))
}

fn read_result<T>(r: Result<T, ReadError>, ok: impl FnOnce(&T) -> Value) -> Value {
    match r {
        Ok(x) => Value::ok(ok(&x)),
        Err(e) => Value::err(wire::read_error_value(&e)),
    }
}

/// One of tmiki's calls. `Err`: a trap (an argument that does not fit).
fn dispatch<P: Plugin>(func: &str, args: &[Value]) -> Result<Value, String> {
    let a = |i| arg(args, i, func);
    let e = |e: ValueError| format!("{func}: {e}");
    Ok(match func {
        "hello" => {
            let v = a(0)?.u64("protocol").map_err(e)?;
            if v != PROTOCOL {
                return Err(format!("tmiki speaks protocol {v}; this SDK speaks {PROTOCOL}"));
            }
            Value::record([("protocol", Value::Uint(PROTOCOL)), ("abi", Value::text(ABI)), ("writer", Value::Bool(true))])
        }
        "reader.info" => wire::plugin_info_value(&P::info()),
        "reader.probe" => {
            let name = a(0)?.str("name").map_err(e)?;
            let head = a(1)?.byte_slice("head").map_err(e)?;
            let size = a(2)?.u64("size").map_err(e)?;
            wire::confidence_value(P::probe(name, head, size))
        }
        "reader.read-structure" => read_result(P::read_structure(&source(a(0)?)?), wire::structure_value),
        "reader.read-layer" => {
            let src = source(a(0)?)?;
            let context = a(1)?.byte_slice("context").map_err(e)?;
            let token = a(2)?.u32("token").map_err(e)?;
            read_result(P::read_layer(&src, context, token), wire::bitmap_value)
        }
        "reader.decode-chunk" => {
            let id = a(0)?.u32("codec-id").map_err(e)?;
            let input = a(1)?.byte_slice("input").map_err(e)?;
            let out_len = a(2)?.u32("out-len").map_err(e)?;
            read_result(P::decode_chunk(id, input, out_len), |b| Value::bytes(b.clone()))
        }
        "writer.write-layers" => {
            let src = source(a(0)?)?;
            let edits = a(1)?.array("edits").map_err(e)?.iter().map(|x| wire::layer_edit(x, "edits[]")).collect::<Result<Vec<_>, _>>().map_err(e)?;
            let pixels = a(2)?
                .array("pixels")
                .map_err(e)?
                .iter()
                .map(|h| h.u32("pixels[]").map(|handle| edit::Pixels(crate::sys::Pixels { handle })))
                .collect::<Result<Vec<_>, _>>()
                .map_err(e)?;
            match P::write_layers(&src, edits, pixels) {
                Ok(plan) => Value::ok(wire::write_plan_value(plan)),
                Err(x) => Value::err(wire::write_error_value(&x)),
            }
        }
        other => return Err(format!("this plugin has no export {other:?}")),
    })
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(t) = p.downcast_ref::<HostTrap>() {
        t.0.clone()
    } else if let Some(s) = p.downcast_ref::<&str>() {
        format!("panic: {s}")
    } else if let Some(s) = p.downcast_ref::<String>() {
        format!("panic: {s}")
    } else {
        "panic".into()
    }
}

/// Runs plugin `P` as a development plugin: answers tmiki's calls on stdin
/// and stdout until stdin ends, then exits. Call it from `main` of a
/// native build (`tmiki plugin dev <that program> -- add a.ora`).
pub fn dev_main<P: Plugin>() -> ! {
    // a host trap is tmiki's own message: not printed again as a panic
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if info.payload().downcast_ref::<HostTrap>().is_none() {
            default_hook(info);
        }
    }));
    *PIPE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pipe { input: BufReader::new(std::io::stdin()), output: BufWriter::new(std::io::stdout()) });
    loop {
        let message = {
            let mut guard = PIPE.lock().unwrap_or_else(|e| e.into_inner());
            receive(guard.as_mut().expect("set above"))
        };
        let reply = match message {
            Message::Call { func, args } => match panic::catch_unwind(AssertUnwindSafe(|| dispatch::<P>(&func, &args))) {
                Ok(Ok(v)) => Message::Return(v),
                Ok(Err(m)) => Message::Trap(m),
                Err(p) => Message::Trap(panic_text(p.as_ref())),
            },
            Message::Import { func, .. } => fatal(&format!("tmiki sent an import ({func}); it sends calls")),
            Message::Return(_) | Message::Trap(_) => fatal("tmiki sent an answer where it should call"),
        };
        let mut guard = PIPE.lock().unwrap_or_else(|e| e.into_inner());
        let pipe = guard.as_mut().expect("set above");
        send(pipe, reply);
        let _ = pipe.output.flush();
    }
}
