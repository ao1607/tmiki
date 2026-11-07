//! A plugin that opens a TCP connection (PLUGINS.md 3.2): Rust's std puts
//! `wasi:sockets` among the component's imports, and the host refuses the
//! component when it is loaded, before any of it runs.

#[cfg(target_arch = "wasm32")]
mod wasm {
    wit_bindgen::generate!({
        path: "../../crates/tmi-format/wit",
        world: "format-reader",
    });

    use exports::tmi::format::reader::Guest;
    use tmi::format::source::SourceFile;
    use tmi::format::types::*;

    struct Sockets;

    impl Guest for Sockets {
        fn info() -> PluginInfo {
            PluginInfo { id: "bad-sockets".into(), version: "0.1.0".into(), formats: vec![], probe_bytes: 0, can_write: false }
        }

        fn probe(_name: String, _head: Vec<u8>, _size: u64) -> Confidence {
            Confidence::No
        }

        fn read_structure(_src: &SourceFile) -> Result<Structure, ReadError> {
            match std::net::TcpStream::connect("127.0.0.1:9") {
                Ok(_) => Err(ReadError::Corrupt("connected".into())),
                Err(e) => Err(ReadError::Corrupt(format!("{e}"))),
            }
        }

        fn read_layer(_src: &SourceFile, _context: Vec<u8>, _token: u32) -> Result<Bitmap, ReadError> {
            Err(ReadError::Unsupported("none".into()))
        }

        fn decode_chunk(_codec_id: u32, _input: Vec<u8>, _out_len: u32) -> Result<Vec<u8>, ReadError> {
            Err(ReadError::Unsupported("none".into()))
        }
    }

    export!(Sockets);
}
