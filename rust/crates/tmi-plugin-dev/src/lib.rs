//! The wire of `tmiki plugin dev` (PLUGINS.md 7.1, P8).
//!
//! A development plugin is an ordinary program (a native build of a Rust
//! plugin through `tmi_plugin_sdk::dev_main`) that speaks the WIT interface `tmi:format@0.1.0`
//! with tmiki over its standard input and output. Nothing here is specific to
//! tmiki's own types; `tmi-plugin-host` (the tmiki side) and the SDKs (the
//! plugin side) convert their types to and from [`Value`].
//!
//! # Frames
//!
//! Each message is one frame: a 4-byte big-endian length `n` (1 to
//! [`MAX_FRAME`]) and `n` bytes holding exactly one CBOR data item (RFC
//! 8949; definite lengths only, no tags). tmiki writes frames to the plugin's
//! stdin and reads them from its stdout. The plugin's stderr is free text
//! (tmiki shows it). A plugin must not print anything else on stdout.
//!
//! # WIT values in CBOR
//!
//! | WIT | CBOR |
//! |---|---|
//! | `bool` | `true` / `false` |
//! | `u8`..`u64`, `s8`..`s64` | integer |
//! | `f64` | float (a reader also takes an integer) |
//! | `string` | text |
//! | `list<u8>` | byte string |
//! | other `list<T>`, `tuple<..>` | array |
//! | `option<T>` | `null`, or the value |
//! | `record` | map from the field names as the WIT spells them (`stable-id`) |
//! | `enum` | text, the case name (`pass-through`) |
//! | `variant` | map of one entry: case name to payload (`null` when the case has none): `{"span": {"offset": 0, "len": 9}}`, `{"deflate": null}` |
//! | `result<T, E>` | `{"ok": T}` or `{"err": E}` |
//! | resource (`source-file`, `pixels`, `blob`, `database`) | the handle's number (unsigned) |
//!
//! # Messages
//!
//! Every message is a CBOR map:
//!
//! - `{"call": name, "args": [..]}`: tmiki calls an export of the plugin. The
//!   names are `<interface>.<function>`: `reader.info`, `reader.probe`,
//!   `reader.read-structure`, `reader.read-layer`, `reader.decode-chunk`,
//!   `writer.write-layers`; and `hello` (below). Arguments are positional,
//!   as the WIT lists them; a `borrow<source-file>` is a handle.
//! - `{"import": name, "args": [..]}`: while it answers a call, the plugin
//!   calls a host function (WIT import): `<interface>.<function>`
//!   (`codecs.transform`, `edit.encode-image`, `log.emit`, ...),
//!   `<interface>.<resource>.<method>` with the handle first
//!   (`source.source-file.read` `[h, offset, len]`), a static function
//!   without one (`sqlite.database.open` `[image]`), and
//!   `<interface>.<resource>.drop` `[h]`.
//! - `{"return": value}`: the answer to the latest open call or import.
//! - `{"trap": message}`: the callee failed (a panic, a bad handle, a value
//!   that does not fit the WIT): the call is over. When tmiki answers an import
//!   with a trap, the plugin abandons the call it is answering and answers it
//!   with a trap too (the SDKs do this).
//!
//! The conversation is strictly nested: after a call, the plugin sends any
//! number of imports (each answered before the next) and then one return or
//! trap. Handles are valid for one call only (each call is a fresh instance
//! for a wasm plugin; PLUGINS.md 3.5): the source file is handle 0, pixels
//! and blobs are numbered by tmiki (`write-layers` hands over pixels `0..n`),
//! databases by tmiki in the order opened.
//!
//! `hello` (`args: [1]`, the protocol version) is the first call. The answer:
//! `{"protocol": 1, "abi": "tmi:format@0.1.0", "writer": bool}` (`writer`:
//! the plugin exports `writer`; tmiki writes through it only when `info` also
//! says `can-write`). The plugin ends when its stdin ends.
//!
//! Records a development plugin makes carry `importer.component_sha256 =
//! "dev"`, and no commit takes them (PLUGINS.md 5).

pub mod cbor;
pub mod frame;
pub mod msg;

pub use cbor::{Value, ValueError};
pub use frame::{read_frame, write_frame, FrameError, MAX_FRAME};
pub use msg::Message;

/// The protocol version `hello` names.
pub const PROTOCOL: u64 = 1;

/// The `component_sha256` of every record a development plugin makes.
pub const DEV_COMPONENT: &str = "dev";
