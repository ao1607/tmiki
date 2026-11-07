//! WebP decode as Pillow 12.3.0 opens and loads it (WebPImagePlugin over
//! `_webp.WebPAnimDecoder`), with the libwebp it bundles (1.6.0, vendored
//! by `libwebp-sys` 0.14.4):
//!
//! - the mode is "RGBA" unless `WebPGetFeatures` says the file has no
//!   alpha ("RGB", the decoder's bytes read as "RGBX");
//! - the pixels are the first frame of `WebPAnimDecoder` with its default
//!   options (RGBA, no threads): the canvas, for an animation frame 0
//!   composed on it, for a still image the image;
//! - `info["icc_profile"]` is the ICCP chunk when it is not empty; WebP
//!   has no dpi in Pillow's `info`.
//!
//! Lossy files decode as libwebp 1.6.0 decodes them, which is what Pillow
//! 12.3's wheels carry (PLUGINS.md names this `webp-v1`).

use libwebp_sys::*;
use tmi_pillow::image::{Image, Mode};

use crate::{DecodeError, Format, Info, Limit, Opened};

/// `_accept`: RIFF, WEBP and a VP8 chunk tag.
pub fn accept(data: &[u8]) -> bool {
    data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") && matches!(data.get(12..16), Some(b"VP8 " | b"VP8X" | b"VP8L"))
}

fn corrupt(m: &str) -> DecodeError {
    DecodeError::Corrupt(Format::WebP, m.to_string())
}

/// An owned `WebPAnimDecoder`.
struct AnimDecoder(*mut WebPAnimDecoder);

impl Drop for AnimDecoder {
    fn drop(&mut self) {
        unsafe { WebPAnimDecoderDelete(self.0) }
    }
}

/// The size `Image.open(data).size` reports.
pub fn size(data: &[u8]) -> Result<(usize, usize), DecodeError> {
    let (_, info) = decoder(data)?;
    Ok((info.canvas_width as usize, info.canvas_height as usize))
}

fn decoder(data: &[u8]) -> Result<(AnimDecoder, WebPAnimInfo), DecodeError> {
    if !accept(data) {
        return Err(corrupt("not a WEBP file"));
    }
    unsafe {
        let src = WebPData { bytes: data.as_ptr(), size: data.len() };
        let dec = WebPAnimDecoderNew(&src, std::ptr::null());
        if dec.is_null() {
            return Err(corrupt("could not create decoder object"));
        }
        let dec = AnimDecoder(dec);
        let mut info: WebPAnimInfo = std::mem::zeroed();
        if WebPAnimDecoderGetInfo(dec.0, &mut info) == 0 {
            return Err(corrupt("could not create decoder object"));
        }
        Ok((dec, info))
    }
}

pub(crate) fn open(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    // _anim_decoder_new sniffs the mode first.
    let has_alpha = unsafe {
        let mut features: WebPBitstreamFeatures = std::mem::zeroed();
        WebPGetFeatures(data.as_ptr(), data.len(), &mut features) != VP8StatusCode::VP8_STATUS_OK || features.has_alpha != 0
    };
    let (dec, info) = decoder(data)?;
    let (w, h) = (info.canvas_width as usize, info.canvas_height as usize);
    if w == 0 || h == 0 {
        return Err(corrupt("not identified by this driver"));
    }
    limit.check(w, h)?;
    let icc = unsafe {
        let demux = WebPAnimDecoderGetDemuxer(dec.0);
        let mut iter: WebPChunkIterator = std::mem::zeroed();
        if WebPDemuxGetChunk(demux, c"ICCP".as_ptr(), 1, &mut iter) != 0 {
            let chunk = std::slice::from_raw_parts(iter.chunk.bytes, iter.chunk.size).to_vec();
            WebPDemuxReleaseChunkIterator(&mut iter);
            Some(chunk).filter(|c| !c.is_empty())
        } else {
            None
        }
    };
    let rgba = unsafe {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut timestamp: i32 = 0;
        if WebPAnimDecoderGetNext(dec.0, &mut buf, &mut timestamp) == 0 || buf.is_null() {
            return Err(corrupt("failed to read next frame"));
        }
        std::slice::from_raw_parts(buf, w * 4 * h).to_vec()
    };
    let image = if has_alpha {
        Image::new(Mode::RGBA, w, h, rgba)
    } else {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for p in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&p[..3]);
        }
        Image::new(Mode::RGB, w, h, rgb)
    };
    Ok(Opened { image, format: Format::WebP, info: Info { dpi: None, icc_profile: icc } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webp_enc::{encode_webp, WebpOptions};

    #[test]
    fn lossless_round_trip() {
        let px: Vec<u8> = (0..4 * 3 * 4).map(|i| (i * 37 % 256) as u8).collect();
        let opts = WebpOptions { lossless: true, exact: true, ..Default::default() };
        let data = encode_webp(&px, 4, 3, 4, &opts).unwrap();
        let o = open(&data, Limit(None)).unwrap();
        assert_eq!((o.image.mode, o.image.width, o.image.height), (Mode::RGBA, 4, 3));
        assert_eq!(o.image.data, px);
    }
}
