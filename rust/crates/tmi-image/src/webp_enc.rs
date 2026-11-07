//! WebP encode with libwebp 1.6.0 (`libwebp-sys`, built from its vendored
//! sources), configured as Pillow 12.3's `_webp.WebPEncode`:
//! `WebPConfigInit` (default preset), then lossless / quality /
//! alpha_quality / method / exact, the picture imported as ARGB
//! (`use_argb = 1`, so libwebp does the RGB -> YUV conversion itself).
//! Pillow then passes the bitstream through WebPMux (no ICC / EXIF / XMP
//! here); this writes the encoder's output as it is.

use libwebp_sys::*;

#[derive(Clone, Debug)]
pub struct WebpOptions {
    pub lossless: bool,
    pub quality: f32,
    pub alpha_quality: f32,
    pub method: i32,
    pub exact: bool,
}

impl Default for WebpOptions {
    /// Pillow's defaults (quality 80, alpha_quality 100, method 4).
    fn default() -> Self {
        WebpOptions { lossless: false, quality: 80.0, alpha_quality: 100.0, method: 4, exact: false }
    }
}

/// `image.save(buf, "WEBP", ...)` of an RGBA (`bands` 4) or RGB (3) image.
pub fn encode_webp(px: &[u8], width: usize, height: usize, bands: usize, opts: &WebpOptions) -> Result<Vec<u8>, String> {
    if !(bands == 3 || bands == 4) || px.len() != width * height * bands {
        return Err("bad image".into());
    }
    unsafe {
        let mut config = WebPConfig::new().map_err(|_| "failed to initialize config")?;
        config.lossless = opts.lossless as i32;
        config.quality = opts.quality;
        config.alpha_quality = opts.alpha_quality as i32;
        config.method = opts.method;
        config.exact = opts.exact as i32;
        if WebPValidateConfig(&config) == 0 {
            return Err("invalid configuration".into());
        }
        let mut pic = WebPPicture::new().map_err(|_| "could not initialise picture")?;
        pic.width = width as i32;
        pic.height = height as i32;
        pic.use_argb = 1;
        if WebPPictureAlloc(&mut pic) == 0 {
            return Err("can't allocate picture frame".into());
        }
        for y in 0..height {
            let dst = std::slice::from_raw_parts_mut(pic.argb.add(y * pic.argb_stride as usize), width);
            let row = &px[y * width * bands..(y + 1) * width * bands];
            for (d, s) in dst.iter_mut().zip(row.chunks_exact(bands)) {
                let a = if bands == 4 { s[3] } else { 0xff };
                *d = s[2] as u32 | (s[1] as u32) << 8 | (s[0] as u32) << 16 | (a as u32) << 24;
            }
        }
        let mut writer: WebPMemoryWriter = std::mem::zeroed();
        WebPMemoryWriterInit(&mut writer);
        pic.writer = Some(WebPMemoryWrite);
        pic.custom_ptr = &mut writer as *mut WebPMemoryWriter as *mut std::ffi::c_void;
        let ok = WebPEncode(&config, &mut pic);
        let code = pic.error_code;
        WebPPictureFree(&mut pic);
        let out = if writer.mem.is_null() { Vec::new() } else { std::slice::from_raw_parts(writer.mem, writer.size).to_vec() };
        WebPMemoryWriterClear(&mut writer);
        if ok == 0 {
            return Err(format!("encoding error {:?}", code));
        }
        Ok(out)
    }
}

/// libwebp's version, as `0xMMmmrr`.
pub fn encoder_version() -> i32 {
    unsafe { WebPGetEncoderVersion() }
}
