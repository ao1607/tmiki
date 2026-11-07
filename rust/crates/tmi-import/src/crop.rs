//! image_ops.py `LayerCrop`: a layer reduced to its painted rectangle, its
//! pixel digest, alpha bbox and TMIL layer blob.

use tmi_store::json::{Int, Value};
use tmi_store::oid::Sha256;

pub type Rect = (usize, usize, usize, usize);

pub struct LayerCrop {
    /// The full layer size.
    pub size: (usize, usize),
    /// Where the crop sits; None for an all-zero layer.
    pub rect: Option<Rect>,
    /// The crop's RGBA rows (empty when `rect` is None).
    pub rgba: Vec<u8>,
}

impl LayerCrop {
    /// `LayerCrop.from_full(image)`: `getbbox(alpha_only=False)` and crop.
    pub fn from_full(rgba: &[u8], w: usize, h: usize) -> LayerCrop {
        let rect = if w == 0 { None } else { tmi_pillow::bbox::bbox_any(rgba, w, 4) };
        match rect {
            None => LayerCrop { size: (w, h), rect: None, rgba: Vec::new() },
            Some(r) => {
                let crop = if r == (0, 0, w, h) {
                    rgba.to_vec()
                } else {
                    tmi_pillow::rgba::crop(rgba, w, h, r.0 as i64, r.1 as i64, r.2 as i64, r.3 as i64)
                };
                LayerCrop { size: (w, h), rect: Some(r), rgba: crop }
            }
        }
    }

    /// `LayerCrop.from_tight_crop(size, x, y, image)`: a crop the decoder
    /// already trimmed (`w == 0 or h == 0`: no image).
    pub fn from_tight(size: (usize, usize), x: usize, y: usize, w: usize, h: usize, rgba: Vec<u8>) -> LayerCrop {
        if w == 0 || h == 0 {
            return LayerCrop { size, rect: None, rgba: Vec::new() };
        }
        LayerCrop { size, rect: Some((x, y, x + w, y + h)), rgba }
    }

    pub fn header(&self) -> String {
        let (w, h) = self.size;
        match self.rect {
            None => format!("v1 {w} {h}"),
            Some((x0, y0, x1, y1)) => format!("v1 {w} {h} {x0} {y0} {x1} {y1}"),
        }
    }

    /// sha256 over "tmi-layer <header>:" and the crop's RGBA.
    pub fn digest(&self) -> String {
        let mut h = Sha256::default();
        h.update(format!("tmi-layer {}:", self.header()).as_bytes());
        h.update(&self.rgba);
        h.hexdigest()
    }

    /// `alpha_bbox()`: the box of non-zero alphas, in layer coordinates.
    pub fn alpha_bbox(&self) -> Option<[usize; 4]> {
        let (x0, y0, x1, _) = self.rect?;
        let b = tmi_pillow::bbox::alpha_bbox(&self.rgba, x1 - x0)?;
        Some([b.0 + x0, b.1 + y0, b.2 + x0, b.3 + y0])
    }

    /// The layer blob (TMIL).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let (lw, lh) = (self.size.0 as u32, self.size.1 as u32);
        match self.rect {
            None => tmi_codec::layer_blob::encode(&[], 0, 0, lw, lh, 0, 0),
            Some((x0, y0, x1, y1)) => tmi_codec::layer_blob::encode(
                &self.rgba,
                (x1 - x0) as u32,
                (y1 - y0) as u32,
                lw,
                lh,
                x0 as u32,
                y0 as u32,
            ),
        }
    }
}

/// `_digest_and_encode(crop, sid, reuse_pixels, layer_blobs)`: the digest,
/// and the blob unless the layer's blob is not wanted or its digest equals
/// `reuse_pixels[sid]`. Without a reuse candidate the encode is needed
/// whatever the digest is, so the two run side by side (one SHA-256 stream
/// cannot be split).
pub fn digest_and_encode(crop: &LayerCrop, sid: &str, opts: &crate::Options) -> crate::Res<(String, Option<Vec<u8>>)> {
    if !opts.layer_blobs.wants(sid) {
        return Ok((crop.digest(), None));
    }
    match opts.reuse_pixels.and_then(|p| p.get(sid)) {
        None => {
            let (digest, blob) = rayon::join(|| crop.digest(), || crop.encode());
            Ok((digest, Some(blob.map_err(crate::decline)?)))
        }
        Some(prior) => {
            let digest = crop.digest();
            if &digest == prior {
                Ok((digest, None))
            } else {
                let blob = crop.encode().map_err(crate::decline)?;
                Ok((digest, Some(blob)))
            }
        }
    }
}

/// A bbox as the JSON list Python stores (None: null).
pub fn bbox_value(b: Option<[usize; 4]>) -> Value {
    match b {
        None => Value::Null,
        Some(b) => Value::Array(b.iter().map(|&v| Value::Int(Int::Small(v as i64))).collect()),
    }
}
