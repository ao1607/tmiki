//! Which files tmiki stores as images (PLUGINS.md P0e): a file is an image
//! exactly when tmiki's own decoders read it, a recognised signature and a full
//! decode with no decline; everything else (a format tmiki has no decoder for,
//! a variant a decoder declines, a file Pillow would refuse or warn about) is
//! an opaque blob. Nothing here asks Pillow, so the verdict does not depend
//! on the installed Pillow version, or on Pillow being installed.
//!
//! The Rust import (`tmi-import`) and `tmi_accel.image_import` (the Python
//! import) both call [`open_for_import`], so they record the same objects.

use crate::header;
use crate::{decode_rgba, decode_rgba_any, DecodeError, Format};
use tmi_pillow::image::Image;

/// A file tmiki reads as an image: its RGBA pixels and the metadata the import
/// records (`canvas_meta`).
#[derive(Debug)]
pub struct Accepted {
    /// RGBA8.
    pub image: Image,
    pub format: Format,
    /// `float(info["dpi"][0])`.
    pub dpi: Option<f64>,
    /// `"icc_profile" in info`.
    pub icc: bool,
}

/// Why a file is not an image.
#[derive(Debug)]
pub enum Reject {
    /// tmiki does not read it: stored as an opaque blob. The text says why
    /// (for reports only).
    Opaque(String),
    /// A readable image with more pixels than the limit: an error (the
    /// caller reports it as the pixel limit does), not an opaque blob.
    TooLarge(usize, usize),
}

/// The verdict on `data`. `max_pixels` is the raster pixel limit (None: no
/// limit); an image over it is [`Reject::TooLarge`] once tmiki has recognised
/// it, before any pixels are allocated.
pub fn open_for_import(data: &[u8], max_pixels: Option<u64>) -> Result<Accepted, Reject> {
    match header::header(data) {
        Err(why) => Err(Reject::Opaque(why)),
        Ok(Some(h)) => {
            // `check_raster_pixels`: an empty side counts as 1.
            let pixels = (h.size.0.max(1) as u128) * (h.size.1.max(1) as u128);
            if max_pixels.is_some_and(|m| m != 0 && pixels > m as u128) {
                return Err(Reject::TooLarge(h.size.0, h.size.1));
            }
            let (image, format) = decode_rgba(data).map_err(|e| Reject::Opaque(format!("tmi_image: {e:?}")))?;
            if format.name() != h.format || (image.width, image.height) != h.size {
                return Err(Reject::Opaque("the decode differs from the header".into()));
            }
            Ok(Accepted { image, format, dpi: h.dpi, icc: h.icc })
        }
        Ok(None) => match decode_rgba_any(data, max_pixels.filter(|&m| m != 0)) {
            Ok(o) => Ok(Accepted { image: o.image, format: o.format, dpi: o.info.canvas_dpi(), icc: o.info.icc_profile.is_some() }),
            Err(DecodeError::TooLarge(w, h)) => Err(Reject::TooLarge(w, h)),
            Err(DecodeError::UnknownFormat) => Err(Reject::Opaque("not a format tmiki reads".into())),
            Err(e) => Err(Reject::Opaque(format!("tmi_image: {e:?}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_damaged_files_are_opaque() {
        for data in [&b""[..], b"hello\n", b"GIF89a\x01\0\x01\0not really", b"\x89PNG\r\n\x1a\nnot really", b"\xff\xd8\xff"] {
            assert!(matches!(open_for_import(data, None), Err(Reject::Opaque(_))), "{data:?}");
        }
    }

    #[test]
    fn a_small_gif_is_an_image() {
        // 1x1 GIF89a, one colour
        let gif: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\xff\0\0\0\0\0,\0\0\0\0\x01\0\x01\0\0\x02\x02D\x01\0;";
        let a = open_for_import(gif, None).expect("a GIF");
        assert_eq!((a.format, a.image.width, a.image.height), (Format::Gif, 1, 1));
        assert!(matches!(open_for_import(gif, Some(0)), Ok(_)));
    }
}
