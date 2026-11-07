//! Sizes a wasm plugin's structure announces, refused by the host before it
//! allocates for them (PLUGINS.md 3.4, 5; the `fmt-*` modes of the bad
//! plugin).

use tmi_format::pipeline::ImportError;
use tmi_format::types::ReadError;

use crate::WasmReader;
use crate::tests::{import_both, plugin};

fn through(what: &str) -> Result<(String, String, Vec<String>), ImportError> {
    import_both(&WasmReader::new(plugin("tmi-plugin-bad")), "x.bad", format!("BAD:{what}\n").as_bytes())
}

#[test]
fn a_tile_announcing_four_gigabytes_is_refused_before_it_is_reserved() {
    match through("fmt-lz4-lie") {
        Err(ImportError::Read(ReadError::Corrupt(m))) => assert!(m.contains("announces"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn raw_pixels_whose_size_wraps_are_refused_without_a_panic() {
    match through("fmt-raw-wrap") {
        Err(ImportError::Read(ReadError::Corrupt(m))) => assert!(m.contains("raw pixels"), "{m}"),
        other => panic!("{other:?}"),
    }
}
