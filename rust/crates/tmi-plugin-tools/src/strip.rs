//! Custom sections a component does not need (PLUGINS.md 7.1, `build`):
//! `producers`, debug information, `target_features` and the like, in the
//! component and in every module and component nested in it. The name
//! sections (`name`, `component-name`) stay: wasmtime names the functions of
//! a trap's backtrace with them. Custom sections never change what a
//! component computes, so this changes its SHA-256 and its size only.
//!
//! The binary is walked section by section (`id`, LEB128 size, payload);
//! the payloads of a core module (component section 1) and a nested
//! component (section 4) are walked in turn. Everything else is copied
//! byte for byte.

const KEEP: [&str; 2] = ["name", "component-name"];

fn leb_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    let mut v: u64 = 0;
    for shift in (0..35).step_by(7) {
        let b = *data.get(*pos).ok_or("a size runs past the end")?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return u32::try_from(v).map_err(|_| "a size over 4 GiB".to_string());
        }
    }
    Err("a size of more than 5 bytes".into())
}

fn put_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// `wasm` without its unneeded custom sections, and the names of those
/// removed (with their sizes).
pub fn strip(wasm: &[u8]) -> Result<(Vec<u8>, Vec<(String, usize)>), String> {
    let mut removed = Vec::new();
    let out = walk(wasm, &mut removed)?;
    Ok((out, removed))
}

fn walk(wasm: &[u8], removed: &mut Vec<(String, usize)>) -> Result<Vec<u8>, String> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return Err("not a WebAssembly binary".into());
    }
    // a module is version 1; a component layer 1 (0x0d 0x00 0x01 0x00)
    let component = wasm[6..8] == [1, 0];
    let mut out = wasm[..8].to_vec();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = leb_u32(wasm, &mut pos)? as usize;
        let end = pos.checked_add(size).filter(|&e| e <= wasm.len()).ok_or("a section runs past the end")?;
        let payload = &wasm[pos..end];
        pos = end;
        if id == 0 {
            let mut p = 0;
            let n = leb_u32(payload, &mut p)? as usize;
            let name = payload.get(p..p + n).ok_or("a custom section's name runs past its end")?;
            let name = String::from_utf8_lossy(name).into_owned();
            if !KEEP.contains(&name.as_str()) {
                removed.push((name, size));
                continue;
            }
        }
        let payload = if component && (id == 1 || id == 4) { walk(payload, removed)? } else { payload.to_vec() };
        out.push(id);
        put_leb(&mut out, payload.len() as u32);
        out.extend_from_slice(&payload);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
        out.push(id);
        put_leb(out, payload.len() as u32);
        out.extend_from_slice(payload);
    }

    fn custom(name: &str, data: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        put_leb(&mut p, name.len() as u32);
        p.extend_from_slice(name.as_bytes());
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn nested_custom_sections_go_and_names_stay() {
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        section(&mut module, 0, &custom("producers", &[1; 200]));
        section(&mut module, 1, &[0]); // an empty type section
        section(&mut module, 0, &custom("name", b"n"));
        let mut component = b"\0asm\x0d\0\x01\0".to_vec();
        section(&mut component, 0, &custom(".debug_info", b"dbg"));
        section(&mut component, 1, &module);
        let (out, removed) = strip(&component).unwrap();
        assert_eq!(removed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [".debug_info", "producers"]);
        let mut want_module = b"\0asm\x01\0\0\0".to_vec();
        section(&mut want_module, 1, &[0]);
        section(&mut want_module, 0, &custom("name", b"n"));
        let mut want = b"\0asm\x0d\0\x01\0".to_vec();
        section(&mut want, 1, &want_module);
        assert_eq!(out, want);
        // stripping again changes nothing
        assert_eq!(strip(&out).unwrap(), (out.clone(), vec![]));
        assert!(strip(b"\0asm\x0d\0\x01\0\x01\x05").is_err());
    }
}
