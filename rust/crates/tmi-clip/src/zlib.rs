//! `zlib.compress(data)` of CPython 3.13 byte for byte: stock zlib 1.3.1
//! (libz-sys, pinned), `deflateInit2(level, Z_DEFLATED, 15, 8,
//! Z_DEFAULT_STRATEGY)`, all input at once, `Z_FINISH`. Python feeds the
//! output through growing buffers; how the output space is handed over does
//! not change what deflate emits (checked against Python by the tests).

use libz_sys as z;

/// `BUFFER_BLOCK_SIZE` of CPython's pycore_blocks_output_buffer.h (3.11+):
/// the sizes of the output blocks `zlib.compress` hands deflate in turn.
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

/// The `zlib.ZLIB_RUNTIME_VERSION` the bytes are tied to.
pub fn version() -> String {
    // SAFETY: zlibVersion returns a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(z::zlibVersion()) }.to_string_lossy().into_owned()
}

/// `zlib.compress(data, level)` (level -1 is zlib's default, 6).
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    assert!(data.len() <= u32::MAX as usize, "one deflate call takes at most 4 GiB");
    // SAFETY: the zlib stream API as zlib_compress_impl uses it; the stream
    // and the block being written stay put across every deflate call.
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
        // Python's _BlocksOutputBuffer: output blocks of growing sizes, and
        // deflate(Z_FINISH) again while a call fills its block. With level
        // 0 the stored blocks follow the space each call is given.
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
