//! zlib as CPython 3.13's `zlib` module: compression with stock zlib 1.3.1
//! (libz-sys pinned to 1.1.24, the zlib CPython 3.13 ships), so an object
//! file tmi-store writes has the bytes `zlib.compress(data, level)` gives.
//!
//! Same code as tmi-clip's `zlib.rs` (kept apart so the CLI does not link
//! SQLite); level 0's stored blocks follow the output space each deflate
//! call gets, so the CPython `_BlocksOutputBuffer` block sizes are kept.

use libz_sys as z;
use std::io::Read;

const BLOCK_SIZES: [usize; 17] = [
    32 << 10,
    64 << 10,
    256 << 10,
    1 << 20,
    4 << 20,
    8 << 20,
    16 << 20,
    16 << 20,
    32 << 20,
    32 << 20,
    32 << 20,
    32 << 20,
    64 << 20,
    64 << 20,
    128 << 20,
    128 << 20,
    256 << 20,
];

extern "C" fn zalloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    // SAFETY: plain C allocation, freed by `zfree`.
    unsafe { libc::malloc(items as usize * size as usize) }
}

extern "C" fn zfree(_opaque: z::voidpf, address: z::voidpf) {
    // SAFETY: `address` came from `zalloc`.
    unsafe { libc::free(address) }
}

/// `zlib.ZLIB_RUNTIME_VERSION`.
pub fn version() -> String {
    // SAFETY: zlibVersion returns a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(z::zlibVersion()) }.to_string_lossy().into_owned()
}

/// `zlib.compress(data, level)`.
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    assert!(data.len() <= u32::MAX as usize, "one deflate call takes at most 4 GiB");
    // SAFETY: the zlib stream API as CPython's zlib_compress_impl uses it.
    unsafe {
        let mut strm = z::z_stream {
            next_in: data.as_ptr() as *mut u8,
            avail_in: data.len() as z::uInt,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        };
        let rc = z::deflateInit2_(
            &mut strm,
            level,
            z::Z_DEFLATED,
            15,
            8,
            z::Z_DEFAULT_STRATEGY,
            z::zlibVersion(),
            std::mem::size_of::<z::z_stream>() as i32,
        );
        assert_eq!(rc, z::Z_OK, "deflateInit2 failed");
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        loop {
            let size = BLOCK_SIZES[blocks.len().min(BLOCK_SIZES.len() - 1)];
            blocks.push(vec![0u8; size]);
            let block = blocks.last_mut().unwrap();
            strm.next_out = block.as_mut_ptr();
            strm.avail_out = size as z::uInt;
            let rc = z::deflate(&mut strm, z::Z_FINISH);
            assert!(rc == z::Z_OK || rc == z::Z_STREAM_END || rc == z::Z_BUF_ERROR, "deflate failed: {rc}");
            if strm.avail_out != 0 {
                let used = size - strm.avail_out as usize;
                block.truncate(used);
                break;
            }
        }
        z::deflateEnd(&mut strm);
        blocks.concat()
    }
}

/// Length of `zlib.compress(data, level)` without keeping the output.
pub fn compressed_len(data: &[u8], level: i32) -> usize {
    compress(data, level).len()
}

/// `zlib.decompress(raw)`: one zlib stream; bytes after its end are ignored.
pub fn decompress(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len().saturating_mul(2));
    flate2::read::ZlibDecoder::new(raw).read_to_end(&mut out)?;
    Ok(out)
}
