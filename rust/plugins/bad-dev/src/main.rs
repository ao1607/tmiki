//! A development plugin that misbehaves on purpose (PLUGINS.md 7.1, the
//! security tests of `tmi-plugin-host/src/hardening_tests.rs`). It answers
//! `hello` and `info`, and what it says or does is its first argument:
//!
//! | argument | does |
//! |---|---|
//! | `ok` | a good `info` (id `bad-dev`, version `0.1.0`, extension `baddev`) |
//! | `version-escape` | a version with CR and an ANSI escape after the `-` |
//! | `version-colon` | version `1.0.0-a:b` (an NTFS stream name if it were a file name) |
//! | `ext-newline` | an extension that carries a newline and a second `.tmiattributes` line |
//! | `ext-glob` | the extension `*` |
//! | `name-escape` | a format name with an ANSI escape |
//! | `no-read` | a good `info`; a `read-structure` call asks the host for the whole source and then never reads its stdin again |
//! | `escape-error` | a good `info`; `read-structure` answers `corrupt` with an ANSI escape in the message |

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native::run();
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::io::{stdin, stdout, Write};
    use tmi_plugin_dev::{read_frame, write_frame, Message, Value, PROTOCOL};

    fn info(mode: &str) -> Value {
        let (mut version, mut ext, mut name) = ("0.1.0".to_string(), "baddev".to_string(), "Bad".to_string());
        match mode {
            "version-escape" => version = "1.0.0-rc\r\x1b[2K\x1b[32msigned and trusted\x1b[0m".into(),
            "version-colon" => version = "1.0.0-a:b".into(),
            "ext-newline" => ext = ".evil\n*.png format=plugin:bad-dev\n*".into(),
            "ext-glob" => ext = "*".into(),
            "name-escape" => name = "Bad\x1b[2J".into(),
            _ => {}
        }
        Value::record([
            ("id", Value::text("bad-dev")),
            ("version", Value::text(version)),
            ("formats", Value::list([ext], |e| Value::record([("name", Value::text(name.clone())), ("extensions", Value::list([e], Value::text)), ("mime", Value::option(None))]))),
            ("probe-bytes", Value::uint(16u32)),
            ("can-write", Value::Bool(false)),
        ])
    }

    pub fn run() {
        let mode = std::env::args().nth(1).unwrap_or_else(|| "ok".into());
        let mut input = stdin().lock();
        let mut output = stdout().lock();
        let mut send = |m: Message| {
            let _ = write_frame(&mut output, &m.into_value());
            let _ = output.flush();
        };
        while let Ok(Some(v)) = read_frame(&mut input) {
            let Ok(Message::Call { func, .. }) = Message::from_value(v) else { return };
            match func.as_str() {
                "hello" => send(Message::Return(Value::record([
                    ("protocol", Value::Uint(PROTOCOL)),
                    ("abi", Value::text("tmi:format@0.1.0")),
                    ("writer", Value::Bool(false)),
                ]))),
                "reader.info" => send(Message::Return(info(&mode))),
                "reader.read-structure" if mode == "no-read" => {
                    // asks for the whole source (its size is the test's) and never reads the answer
                    send(Message::Import { func: "source.source-file.read".into(), args: vec![Value::Uint(0), Value::Uint(0), Value::Uint(u32::MAX as u64)] });
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(120));
                    }
                }
                "reader.read-structure" if mode == "escape-error" => {
                    send(Message::Return(Value::err(Value::case("corrupt", Value::text("bad\r\x1b[2K\x1b[32mtrusted\x1b[0m\u{202e}")))))
                }
                other => send(Message::Trap(format!("bad-dev has no {other}"))),
            }
        }
    }
}
