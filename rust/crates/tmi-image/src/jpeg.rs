//! JPEG through libjpeg-turbo's code (the `mozjpeg-sys` build), set up as
//! Pillow 12.3's JpegDecode.c / JpegEncode.c set up libjpeg-turbo.
//!
//! Decode (`Image.open` then `load`, no `draft`): `jpeg_read_header`, the
//! library's colour-space guess kept (Pillow passes no jpegmode), the
//! output colour space from the mode the SOF gives (1 component "L" ->
//! grey, 3 "RGB" -> RGB, 4 "CMYK" -> CMYK, every byte then inverted: Pillow
//! reads CMYK as "CMYK;I", the Adobe convention, whether or not the file has
//! an Adobe marker), and the library defaults otherwise: ISLOW IDCT, fancy
//! upsampling, block smoothing. Not 8-bit, or not 1 / 3 / 4 components:
//! refused, as Pillow does. A file that ends early is an error (Pillow:
//! "image file is truncated"), not the grey fill libjpeg would give. Any
//! other libjpeg warning (corrupt entropy data and the like) declines the
//! file: libjpeg-turbo releases recover from corrupt data differently, so
//! the pixels would not be guaranteed to be Pillow's.
//!
//! Encode: `jpeg_c_set_int_param(JINT_COMPRESS_PROFILE, JCP_FASTEST)`, for
//! progressive output too, so the output is plain libjpeg-turbo's (Pillow's
//! wheels), then `jpeg_set_defaults`, the quality or the given
//! quantisation tables (`qtables=` / `quality="keep"`), the subsampling.

use std::ffi::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};

use mozjpeg_sys::*;
use tmi_pillow::image::{Image, Mode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpegError {
    /// libjpeg's message.
    Libjpeg(String),
    /// What Pillow refuses: not 8-bit, or not 1 / 3 / 4 components.
    Unsupported(String),
    Truncated,
    /// libjpeg warned (corrupt data) this many times: declined, not exact.
    Corrupt(u32, String),
    /// More pixels (width, height) than the caller's limit, seen in the
    /// header before libjpeg allocates anything for them.
    TooLarge(usize, usize),
}

struct Panic(JpegError);

/// The message template of the current code (the crate's code constants
/// follow the jpeg6b numbering, not this jpeg8 build's).
unsafe fn message_template(err: &jpeg_error_mgr) -> String {
    let code = err.msg_code;
    if code < 0 || code > err.last_jpeg_message || err.jpeg_message_table.is_null() {
        return format!("libjpeg message {code}");
    }
    let p = *err.jpeg_message_table.add(code as usize);
    if p.is_null() {
        return format!("libjpeg message {code}");
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

unsafe extern "C-unwind" fn error_exit(cinfo: &mut jpeg_common_struct) {
    let msg = message_template(&*cinfo.err);
    std::panic::resume_unwind(Box::new(Panic(JpegError::Libjpeg(msg))));
}

thread_local! {
    static FIRST_WARNING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "C-unwind" fn emit_message(cinfo: &mut jpeg_common_struct, level: c_int) {
    let err = &mut *cinfo.err;
    if level < 0 {
        let msg = message_template(err);
        // JWRN_JPEG_EOF: the data ran out and libjpeg would pad.
        if msg.starts_with("Premature end of JPEG file") {
            std::panic::resume_unwind(Box::new(Panic(JpegError::Truncated)));
        }
        FIRST_WARNING.with(|w| {
            let mut w = w.borrow_mut();
            if w.is_none() {
                *w = Some(msg);
            }
        });
        err.num_warnings += 1;
    }
}

unsafe extern "C-unwind" fn output_message(_cinfo: &mut jpeg_common_struct) {}

fn catch<T>(f: impl FnOnce() -> Result<T, JpegError>) -> Result<T, JpegError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => match payload.downcast::<Panic>() {
            Ok(p) => Err(p.0),
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}

fn error_mgr() -> Box<jpeg_error_mgr> {
    let mut err: Box<jpeg_error_mgr> = Box::new(unsafe { std::mem::zeroed() });
    unsafe { jpeg_std_error(&mut err) };
    err.error_exit = Some(error_exit);
    err.emit_message = Some(emit_message);
    err.output_message = Some(output_message);
    err
}

/// The JPEG's frame header: (precision, components, sampling factors (h, v)
/// per component), from the first SOF marker.
pub fn frame_info(data: &[u8]) -> Option<(u8, u8, Vec<(u8, u8)>)> {
    let mut pos = 2;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        if marker == 0xFF || marker == 0x00 || (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            pos += if marker == 0xFF { 1 } else { 2 };
            continue;
        }
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let seg = data.get(pos + 4..pos + 2 + len)?;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let n = *seg.get(5)?;
            let samp = (0..n as usize).map(|i| seg.get(6 + i * 3 + 1).map(|&v| (v >> 4, v & 15))).collect::<Option<Vec<_>>>()?;
            return Some((seg[0], n, samp));
        }
        if marker == 0xDA {
            return None;
        }
        pos += 2 + len;
    }
    None
}

/// The quantisation tables of a JPEG in natural order, by table id, as
/// `JpegImageFile.quantization` holds them (8- or 16-bit entries).
pub fn quantization(data: &[u8]) -> Vec<(u8, [u16; 64])> {
    const ZIGZAG: [usize; 64] = [
        0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56,
        57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
    ];
    let mut out: Vec<(u8, [u16; 64])> = Vec::new();
    let mut pos = 2;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        if marker == 0xFF || marker == 0x00 || (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            pos += if marker == 0xFF { 1 } else { 2 };
            continue;
        }
        if marker == 0xDA {
            break;
        }
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let Some(seg) = data.get(pos + 4..pos + 2 + len) else { break };
        if marker == 0xDB {
            let mut s = seg;
            while !s.is_empty() {
                let (prec, id) = (s[0] >> 4, s[0] & 15);
                let size = if prec == 0 { 64 } else { 128 };
                let Some(body) = s.get(1..1 + size) else { break };
                let mut zz = [0u16; 64];
                for (i, v) in zz.iter_mut().enumerate() {
                    *v = if prec == 0 { body[i] as u16 } else { u16::from_be_bytes([body[2 * i], body[2 * i + 1]]) };
                }
                // Pillow: quantization[id] = [data[i] for i in zigzag_index]
                let mut natural = [0u16; 64];
                for (k, &z) in ZIGZAG.iter().enumerate() {
                    natural[z] = zz[k];
                }
                out.retain(|(i, _)| *i != id);
                out.push((id, natural));
                s = &s[1 + size..];
            }
        }
        pos += 2 + len;
    }
    out
}

/// The pre-quantisation size of the largest coefficient of the file,
/// `max(|coefficient| * quantiser - quantiser / 2)` (its entropy data decoded
/// once more, by `jpeg_read_coefficients`), or None where libjpeg fails.
/// An 8-bit DCT never exceeds 1024; larger values come from damaged data, and
/// libjpeg-turbo's SIMD inverse DCT (the wheels' Pillow) wraps those in 16
/// bits where the C code of this build does not (found by the P0d study: a
/// file with no libjpeg warning at all decoded to other pixels).
pub const MAX_DCT: u32 = 1024 + 8;

pub fn max_dequantized(data: &[u8]) -> Option<u32> {
    FIRST_WARNING.with(|w| *w.borrow_mut() = None);
    let mut err = error_mgr();
    let mut cinfo: Box<jpeg_decompress_struct> = Box::new(unsafe { std::mem::zeroed() });
    cinfo.common.err = &mut *err;
    unsafe { jpeg_create_decompress(&mut *cinfo) };
    let result = catch(|| unsafe {
        jpeg_mem_src(&mut cinfo, data.as_ptr(), data.len() as _);
        jpeg_read_header(&mut cinfo, 1);
        let arrays = jpeg_read_coefficients(&mut cinfo);
        let mut max = 0u32;
        let access = (*cinfo.common.mem).access_virt_barray.expect("memory manager");
        for ci in 0..cinfo.num_components as usize {
            let comp = &*cinfo.comp_info.add(ci);
            let q = &(*comp.quant_table).quantval;
            for row in 0..comp.height_in_blocks {
                let blocks = access(&mut cinfo.common, *arrays.add(ci), row, 1, 0);
                let r = *blocks;
                for b in 0..comp.width_in_blocks as usize {
                    let blk = &*r.add(b);
                    for k in 0..64 {
                        let v = (blk[k] as i32).unsigned_abs() * q[k] as u32;
                        max = max.max(v.saturating_sub(q[k] as u32 / 2));
                    }
                }
            }
        }
        Ok(max)
    });
    unsafe { jpeg_destroy_decompress(&mut cinfo) };
    result.ok()
}

/// `Image.open(jpeg)` and its pixels: mode "L", "RGB" or "CMYK".
pub fn decode_jpeg(data: &[u8]) -> Result<Image, JpegError> {
    decode_jpeg_with(data, false)
}

/// `decode_jpeg`; with `lenient` the pixels libjpeg-turbo recovers to are
/// returned where it warned (a study of mozjpeg-sys against Pillow's
/// libjpeg-turbo: tmi-conform's `--lenient`; the pipeline never sets it).
/// A file that ends early is still `Truncated`.
#[doc(hidden)]
pub fn decode_jpeg_with(data: &[u8], lenient: bool) -> Result<Image, JpegError> {
    decode_jpeg_inner(data, lenient, None)
}

/// `decode_jpeg` held to `max_pixels` (`TooLarge` above it).
pub fn decode_jpeg_max(data: &[u8], max_pixels: Option<u64>) -> Result<Image, JpegError> {
    decode_jpeg_inner(data, false, max_pixels)
}

fn decode_jpeg_inner(data: &[u8], lenient: bool, max_pixels: Option<u64>) -> Result<Image, JpegError> {
    if let Some((bits, comps, _)) = frame_info(data) {
        if bits != 8 {
            return Err(JpegError::Unsupported(format!("cannot handle {bits}-bit layers")));
        }
        if !matches!(comps, 1 | 3 | 4) {
            return Err(JpegError::Unsupported(format!("cannot handle {comps}-layer images")));
        }
    }
    FIRST_WARNING.with(|w| *w.borrow_mut() = None);
    let mut err = error_mgr();
    let mut cinfo: Box<jpeg_decompress_struct> = Box::new(unsafe { std::mem::zeroed() });
    cinfo.common.err = &mut *err;
    unsafe { jpeg_create_decompress(&mut *cinfo) };
    let result = catch(|| unsafe {
        jpeg_mem_src(&mut cinfo, data.as_ptr(), data.len() as _);
        jpeg_read_header(&mut cinfo, 1);
        let (iw, ih) = (cinfo.image_width as usize, cinfo.image_height as usize);
        if max_pixels.is_some_and(|m| (iw as u64) * (ih as u64) > m) {
            return Err(JpegError::TooLarge(iw, ih));
        }
        let (mode, out_cs) = match cinfo.num_components {
            1 => (Mode::L, J_COLOR_SPACE::JCS_GRAYSCALE),
            3 => (Mode::RGB, J_COLOR_SPACE::JCS_RGB),
            4 => (Mode::CMYK, J_COLOR_SPACE::JCS_CMYK),
            n => return Err(JpegError::Unsupported(format!("cannot handle {n}-layer images"))),
        };
        cinfo.out_color_space = out_cs;
        jpeg_start_decompress(&mut cinfo);
        let (w, h) = (cinfo.output_width as usize, cinfo.output_height as usize);
        let comps = cinfo.output_components as usize;
        if comps != mode.bytes_per_pixel() {
            return Err(JpegError::Unsupported(format!("{comps} output components")));
        }
        let stride = w * comps;
        let mut px = vec![0u8; stride * h];
        while (cinfo.output_scanline as usize) < h {
            let y = cinfo.output_scanline as usize;
            let mut rows = [px[y * stride..].as_mut_ptr()];
            if jpeg_read_scanlines(&mut cinfo, rows.as_mut_ptr(), 1) != 1 {
                return Err(JpegError::Truncated);
            }
        }
        jpeg_finish_decompress(&mut cinfo);
        let warned = (*cinfo.common.err).num_warnings;
        if warned > 0 && !lenient {
            // Corrupt entropy data: libjpeg-turbo versions recover from it
            // differently, so these pixels are not guaranteed to be
            // Pillow's. The caller decodes with Pillow instead.
            let first = FIRST_WARNING.with(|w| w.borrow_mut().take()).unwrap_or_default();
            return Err(JpegError::Corrupt(warned as u32, first));
        }
        if !lenient && !max_dequantized(data).is_some_and(|m| m <= MAX_DCT) {
            return Err(JpegError::Corrupt(0, "coefficients no 8-bit DCT gives".into()));
        }
        if mode == Mode::CMYK {
            px.iter_mut().for_each(|v| *v = !*v);
        }
        Ok(Image::new(mode, w, h, px))
    });
    unsafe { jpeg_destroy_decompress(&mut cinfo) };
    result
}

/// What `Image.save(fp, "JPEG", ...)` is given.
#[derive(Clone, Debug)]
pub struct JpegOptions {
    /// `quality=` (-1: the library default 75).
    pub quality: i32,
    /// `qtables=`: tables in natural order, ids 0.. in order.
    pub qtables: Option<Vec<[u16; 64]>>,
    /// `subsampling=`: -1 library default (4:2:0), 0 4:4:4, 1 4:2:2, 2 4:2:0.
    pub subsampling: i32,
    pub optimize: bool,
    pub progressive: bool,
    /// Beyond Pillow (to write test files): explicit (h, v) sampling
    /// factors per component, a restart interval in MCUs, arithmetic coding.
    pub sampling_factors: Option<Vec<(i32, i32)>>,
    pub restart_interval: u32,
    pub arithmetic: bool,
    /// `keep_rgb=True`: an RGB image is stored as RGB, not YCbCr (an Adobe
    /// marker instead of JFIF unless `dpi` asks for JFIF too).
    pub keep_rgb: bool,
    /// `dpi=`, rounded: written as a JFIF density in dots per inch when both
    /// are positive.
    pub dpi: (i32, i32),
    /// Markers written after the file header, in order: (marker code, data
    /// without the length). Pillow writes `exif` (APP1), then its `extra`
    /// (XMP as APP1, the ICC profile as APP2 segments), then `comment` (COM).
    pub markers: Vec<(u8, Vec<u8>)>,
}

impl Default for JpegOptions {
    fn default() -> Self {
        JpegOptions {
            quality: -1,
            qtables: None,
            subsampling: -1,
            optimize: false,
            progressive: false,
            sampling_factors: None,
            restart_interval: 0,
            arithmetic: false,
            keep_rgb: false,
            dpi: (0, 0),
            markers: Vec::new(),
        }
    }
}

/// `image.save(buf, "JPEG", **options)` of an "L" or "RGB" image.
pub fn encode_jpeg(img: &Image, opts: &JpegOptions) -> Result<Vec<u8>, JpegError> {
    let (comps, in_cs) = match img.mode {
        Mode::L => (1, J_COLOR_SPACE::JCS_GRAYSCALE),
        Mode::RGB => (3, J_COLOR_SPACE::JCS_RGB),
        Mode::CMYK => (4, J_COLOR_SPACE::JCS_CMYK),
        m => return Err(JpegError::Unsupported(format!("cannot write mode {} as JPEG", m.name()))),
    };
    let mut err = error_mgr();
    let mut cinfo: Box<jpeg_compress_struct> = Box::new(unsafe { std::mem::zeroed() });
    cinfo.common.err = &mut *err;
    unsafe { jpeg_create_compress(&mut *cinfo) };
    let mut outbuf: *mut u8 = std::ptr::null_mut();
    let mut outsize: std::ffi::c_ulong = 0;
    let result = catch(|| unsafe {
        jpeg_mem_dest(&mut cinfo, &mut outbuf, &mut outsize);
        cinfo.image_width = img.width as _;
        cinfo.image_height = img.height as _;
        cinfo.input_components = comps;
        cinfo.in_color_space = in_cs;
        // Pillow's wheels are built on libjpeg-turbo, which has no
        // compression profiles: progressive or not, they write what MozJPEG
        // writes with JCP_FASTEST. (Pillow skips the call for progressive
        // output only when it is itself built on MozJPEG.)
        jpeg_c_set_int_param(&mut cinfo, JINT_COMPRESS_PROFILE, JCP_FASTEST as c_int);
        jpeg_set_defaults(&mut cinfo);
        if opts.keep_rgb && img.mode == Mode::RGB {
            if !matches!(opts.subsampling, -1 | 0) {
                return Err(JpegError::Unsupported("keep_rgb with chroma subsampling".into()));
            }
            jpeg_set_colorspace(&mut cinfo, J_COLOR_SPACE::JCS_RGB);
        }
        if let Some(tables) = &opts.qtables {
            let (quality, force) = if opts.quality != -1 { (opts.quality, 1) } else { (50, 0) };
            let scale = jpeg_quality_scaling(quality);
            let mut last = 0;
            for (i, t) in tables.iter().enumerate() {
                let basic: Vec<std::ffi::c_uint> = t.iter().map(|&v| v as _).collect();
                jpeg_add_quant_table(&mut cinfo, i as c_int, basic.as_ptr(), scale, force);
                if i < comps as usize {
                    (*cinfo.comp_info.add(i)).quant_tbl_no = i as c_int;
                }
                last = i;
            }
            if tables.len() == 1 {
                let basic: Vec<std::ffi::c_uint> = tables[0].iter().map(|&v| v as _).collect();
                jpeg_add_quant_table(&mut cinfo, 1, basic.as_ptr(), scale, force);
            }
            for i in last..comps as usize {
                (*cinfo.comp_info.add(i)).quant_tbl_no = last as c_int;
            }
        } else if opts.quality != -1 {
            jpeg_set_quality(&mut cinfo, opts.quality, 1);
        }
        let factors: Option<[(c_int, c_int); 3]> = match opts.subsampling {
            0 => Some([(1, 1), (1, 1), (1, 1)]),
            1 => Some([(2, 1), (1, 1), (1, 1)]),
            2 => Some([(2, 2), (1, 1), (1, 1)]),
            _ => None,
        };
        if let Some(f) = factors {
            if comps >= 3 {
                for (i, (h, v)) in f.iter().enumerate() {
                    let c = &mut *cinfo.comp_info.add(i);
                    c.h_samp_factor = *h;
                    c.v_samp_factor = *v;
                }
            } else {
                // Pillow writes comp_info[0..3] regardless; for one
                // component only the first matters.
                let c = &mut *cinfo.comp_info;
                c.h_samp_factor = f[0].0;
                c.v_samp_factor = f[0].1;
            }
        }
        if let Some(f) = &opts.sampling_factors {
            for (i, &(h, v)) in f.iter().enumerate().take(comps as usize) {
                let c = &mut *cinfo.comp_info.add(i);
                c.h_samp_factor = h;
                c.v_samp_factor = v;
            }
        }
        if opts.progressive {
            jpeg_simple_progression(&mut cinfo);
        }
        cinfo.optimize_coding = opts.optimize as boolean;
        cinfo.restart_interval = opts.restart_interval;
        cinfo.arith_code = opts.arithmetic as boolean;
        if opts.dpi.0 > 0 && opts.dpi.1 > 0 {
            cinfo.write_JFIF_header = 1;
            cinfo.density_unit = 1;
            cinfo.X_density = opts.dpi.0 as _;
            cinfo.Y_density = opts.dpi.1 as _;
        }
        jpeg_start_compress(&mut cinfo, 1);
        for (code, data) in &opts.markers {
            jpeg_write_marker(&mut cinfo, *code as c_int, data.as_ptr(), data.len() as _);
        }
        let stride = img.width * comps as usize;
        for y in 0..img.height {
            let rows = [img.data[y * stride..].as_ptr()];
            jpeg_write_scanlines(&mut cinfo, rows.as_ptr(), 1);
        }
        jpeg_finish_compress(&mut cinfo);
        Ok(())
    });
    unsafe { jpeg_destroy_compress(&mut cinfo) };
    let out = if !outbuf.is_null() {
        let v = unsafe { std::slice::from_raw_parts(outbuf, outsize as usize) }.to_vec();
        unsafe { libc::free(outbuf as *mut libc::c_void) };
        v
    } else {
        Vec::new()
    };
    result.map(|_| out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_round_trip_and_truncation() {
        let (w, h) = (37usize, 21usize);
        let px: Vec<u8> = (0..w * h * 3).map(|i| ((i / 3 % w) * 3 + (i / 3 / w) * 5 + (i % 3) * 20) as u8).collect();
        let img = Image::new(Mode::RGB, w, h, px.clone());
        let data = encode_jpeg(&img, &JpegOptions { quality: 95, ..Default::default() }).unwrap();
        let back = decode_jpeg(&data).unwrap();
        assert_eq!((back.mode, back.width, back.height), (Mode::RGB, w, h));
        let err: i64 = back.data.iter().zip(&px).map(|(&a, &b)| (a as i64 - b as i64).abs()).max().unwrap();
        assert!(err < 12, "{err}");
        assert_eq!(decode_jpeg(&data[..data.len() / 2]), Err(JpegError::Truncated));
        let q = quantization(&data);
        assert_eq!(q.len(), 2);
        assert_eq!(frame_info(&data).unwrap().1, 3);
    }

    /// A small JPEG whose frame header says 65000 x 65000 (12 GB of RGB):
    /// refused on the header libjpeg reads, before its output is allocated.
    #[test]
    fn a_jpeg_declaring_billions_of_pixels_is_refused_before_its_pixels() {
        let img = Image::new(Mode::RGB, 8, 8, vec![90; 8 * 8 * 3]);
        let mut data = encode_jpeg(&img, &JpegOptions::default()).unwrap();
        let sof = data.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
        data[sof + 5..sof + 9].copy_from_slice(&[0xFD, 0xE8, 0xFD, 0xE8]);
        assert_eq!(decode_jpeg_max(&data, Some(1 << 20)).err(), Some(JpegError::TooLarge(65000, 65000)));
        assert!(decode_jpeg_max(&encode_jpeg(&img, &JpegOptions::default()).unwrap(), Some(1 << 20)).is_ok());
    }
}
