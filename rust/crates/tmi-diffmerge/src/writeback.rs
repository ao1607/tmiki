//! tmi/native.py `replace_layer_pixels`: one layer's pixels written back
//! into a CLIP, MDP2, PNG or JPEG file (merge, `restore --ours/--theirs
//! file:layer`, `restore --in-place file:layer`). CLIP and MDP2 go through
//! tmi-clip (stage 2c), PNG and JPEG through tmi-image with
//! image_ops.py `encode_raster_like`'s metadata rules (issue #21): the
//! bytes are what Python writes. An original tmiki cannot read as a PNG or
//! JPEG (it would be an opaque blob) keeps no metadata, in both.

use crate::pix::{self, fwd, Px};
use flate2::{Decompress, FlushDecompress, Status};
use tmi_import::header::PyNum;
use tmi_import::{Limits, Warnings};
use tmi_pillow::image::{Image, Mode, Transparency};

use tmi_repo::{RepoError, Result};

/// A Python `ValueError` with this message.
fn value_err(msg: impl Into<String>) -> RepoError {
    RepoError::Value(msg.into())
}

fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// A name casefolded with its whitespace collapsed (`collapse`): ASCII as
/// Python's `casefold()` gives it, other names through `casefold_approx`.
fn ascii_fold(text: &str, collapse: impl Fn(&str) -> String) -> Result<Option<String>> {
    if text.chars().all(|c| c.is_ascii() || is_py_space(c)) {
        let replaced: String = text.chars().map(|c| if is_py_space(c) { ' ' } else { c }).collect();
        return Ok(Some(collapse(&replaced).to_ascii_lowercase()));
    }
    // every other name folds here too (kana and kanji have no case)
    let replaced: String = text.chars().map(|c| if is_py_space(c) { ' ' } else { c }).collect();
    Ok(Some(tmi_repo::py::casefold_approx(&collapse(&replaced))))
}

/// clip.py `_norm`: `" ".join(value.strip().casefold().split())` (ASCII).
fn clip_norm(text: &str) -> Result<Option<String>> {
    ascii_fold(text, |s| s.split(' ').filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" "))
}

/// mdp2.py: `re.sub(r"\s+", " ", text).casefold()` (ASCII).
fn mdp2_norm(text: &str) -> Result<Option<String>> {
    ascii_fold(text, |s| {
        let mut out = String::with_capacity(s.len());
        let mut space = false;
        for c in s.chars() {
            if c == ' ' {
                if !space {
                    out.push(' ');
                }
                space = true;
            } else {
                out.push(c);
                space = false;
            }
        }
        out
    })
}

/// What a write-back reads besides the file: the image-size limits and the
/// warnings of this command (Python's dedupe is per process).
pub struct WriteCtx<'a> {
    pub limits: Limits,
    pub warns: &'a mut Warnings,
}

/// `native.replace_layer_pixels(path, source_format, selector, png_bytes,
/// invalidate_preview_on_noop=...)`. `name` is the file's name (its
/// suffix decides with `source_format`), `current` the file's bytes (None:
/// no file).
pub fn replace_layer_pixels(
    ctx: &mut WriteCtx,
    name: &str,
    current: Option<&[u8]>,
    source_format: &str,
    selector: &str,
    png_bytes: &[u8],
    invalidate_preview_on_noop: bool,
) -> Result<Vec<u8>> {
    let suffix = tmi_import::path_suffix(name).to_lowercase();
    let fmt = source_format.to_lowercase();
    let missing = || RepoError::forward("write-back into a missing file (Python's FileNotFoundError)");
    if let Some(id) = fmt.strip_prefix("plugin:") {
        // a plugin writes through its repository's component, with consent
        // and the check after writing: `plugin_replace`
        return Err(RepoError::runtime(format!("{name}: plugin '{id}' writes layers back only inside its repository")));
    }
    if fmt == "clip" || suffix == ".clip" {
        return clip_replace(ctx, current.ok_or_else(missing)?, selector, png_bytes, invalidate_preview_on_noop);
    }
    if fmt == "mdp2" || suffix == ".mdp" {
        return mdp2_replace(ctx, current.ok_or_else(missing)?, selector, png_bytes);
    }
    if fmt == "png" || fmt == "jpeg" || matches!(suffix.as_str(), ".png" | ".jpg" | ".jpeg") {
        let jpeg = fmt == "jpeg" || (fmt != "png" && fmt != "jpeg" && (suffix == ".jpg" || suffix == ".jpeg"));
        let image = pix::pixels_from_bytes(png_bytes, &ctx.limits)?.into_dense();
        return encode_raster_like(&image, current.filter(|c| !c.is_empty()), jpeg);
    }
    Err(value_err(format!("native layer write-back is not implemented for {source_format}")))
}

/// Layers of a plugin-format file written back (PLUGINS.md P7): `layers`
/// are (stable id, the layer's pixels as a stored layer blob or a PNG), all
/// written in one call of the plugin's writer, with the user's consent and
/// the check after writing (`tmi_repo::formats::write_layers`). The new
/// bytes of `rel` (`current` holds it now; `name` is its last component).
#[allow(clippy::too_many_arguments)]
pub fn plugin_replace(
    repo: &tmi_repo::Repo,
    formats: &tmi_repo::formats::Formats,
    limits: Limits,
    warns: &mut Warnings,
    rel: &str,
    name: &str,
    current: &[u8],
    id: &str,
    layers: &[(String, Vec<u8>)],
) -> Result<Vec<u8>> {
    let loaded = formats.plugin(repo, id).map_err(|u| RepoError::runtime(tmi_repo::formats::not_available_message(rel, id, &u)))?;
    let mut edits = Vec::with_capacity(layers.len());
    for (sid, bytes) in layers {
        edits.push(tmi_format::write::Edit { stable_id: sid.clone(), pixels: pix::pixels_from_bytes(bytes, &limits)?.into_dense() });
    }
    tmi_repo::formats::write_layers(repo, &loaded, rel, name, current, edits, limits, warns)
}

fn walk<'a>(nodes: &'a [tmi_import::clip::Node], out: &mut Vec<&'a tmi_import::clip::Node>) {
    for n in nodes {
        out.push(n);
        walk(&n.children, out);
    }
}

/// clip.py `find_layer_node(clip, selector)` of a CLIP file: (main id,
/// is a folder).
pub fn clip_node_id(raw: &[u8], selector: &str) -> Result<Option<(i64, bool)>> {
    let file = tmi_import::clip::read(raw).map_err(|d| fwd(format!("CLIP open: {}", d.0)))?;
    Ok(find_clip_node(&file, selector)?.map(|n| (n.main_id, n.folder != 0)))
}

/// clip.py `find_layer_node(clip, selector)`.
fn find_clip_node<'a>(file: &'a tmi_import::clip::ClipFile, selector: &str) -> Result<Option<&'a tmi_import::clip::Node>> {
    let Some(normalized) = clip_norm(selector)? else { return Err(fwd("a non-ASCII CLIP layer selector")) };
    let mut nodes = Vec::new();
    walk(&file.root_children, &mut nodes);
    let mut id_matches = Vec::new();
    let mut name_matches = Vec::new();
    for node in nodes {
        let by_id = clip_norm(&format!("clip:{}", node.main_id))?.as_deref() == Some(normalized.as_str());
        let by_uuid = match node.uuid.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => clip_norm(u)?.as_deref() == Some(normalized.as_str()),
            None => false,
        };
        if by_id || by_uuid {
            id_matches.push(node);
        } else if let Some(name) = node.name.as_deref().filter(|n| !n.is_empty()) {
            if clip_norm(name)?.as_deref() == Some(normalized.as_str()) {
                name_matches.push(node);
            }
        }
    }
    match (id_matches.first(), name_matches.len()) {
        (Some(n), _) => Ok(Some(*n)),
        (None, 0) => Ok(None),
        (None, 1) => Ok(Some(name_matches[0])),
        (None, k) => Err(value_err(format!(
            "CLIP layer selector {} is ambiguous: {k} layers share that name. Rename the layers so each name is unique, \
             or select by clip:<id> or layer uuid.",
            crate::pix::py_repr(selector)?
        ))),
    }
}

/// clip.py `replace_layer_pixels` (with `find_layer_node`).
fn clip_replace(ctx: &mut WriteCtx, raw: &[u8], selector: &str, png_bytes: &[u8], invalidate: bool) -> Result<Vec<u8>> {
    let file = tmi_import::clip::read(raw).map_err(|d| fwd(format!("CLIP open: {}", d.0)))?;
    let Some(node) = find_clip_node(&file, selector)? else {
        return Err(value_err(format!("CLIP layer not found: {selector}")));
    };
    let Some(info) = file.leaf_infos.get(&node.main_id) else {
        return Err(fwd("write-back into a CLIP folder node"));
    };
    let Some(info) = info else {
        return Err(value_err(format!("CLIP layer has no writable raster offscreen: {selector}")));
    };
    if !file.extas.contains_key(&info.outer_id) {
        return Err(value_err(format!("CLIP layer has no backed Exta payload: {selector}")));
    }
    let attr = tmi_clip::attr::parse_offscreen_attribute(&info.attribute).map_err(|e| fwd(format!("Offscreen attribute: {e}")))?;
    if attr.channel_profile != "color-alpha-bgra" {
        return Err(value_err(format!("CLIP write-back supports color raster layers only, got {}", attr.channel_profile)));
    }
    // The write-back holds the whole offscreen (twice): its declared size is
    // held to the limit and to its canvas, as the import holds it.
    ctx.limits.check_layered(attr.pixel_width as i64, attr.pixel_height as i64).map_err(value_err)?;
    if let Some(why) = tmi_clip::tiles::offscreen_oversize(attr.pixel_width, attr.pixel_height, file.width.max(0) as usize, file.height.max(0) as usize) {
        return Err(value_err(why));
    }
    let image = pix::pixels_from_bytes(png_bytes, &ctx.limits)?;
    let (w, h) = image.size();
    if w > attr.pixel_width || h > attr.pixel_height {
        return Err(value_err(format!(
            "replacement image {w}x{h} is larger than layer offscreen {}x{}",
            attr.pixel_width, attr.pixel_height
        )));
    }
    let rgba = image.into_dense();
    let out = tmi_clip::write::replace_layer_pixels(raw, info.offscreen_id, &info.attribute, &info.outer_id, &rgba.data, w, h, invalidate)
        .map_err(value_err)?;
    if let Some(why) = &out.decode_failure {
        ctx.warns.offscreen(&info.outer_id, why);
    }
    for (bpt, pixels) in &out.unknown_sizes {
        ctx.warns.tile_size(*bpt, *pixels);
    }
    Ok(out.bytes.unwrap_or_else(|| raw.to_vec()))
}

/// mdp2.py `replace_layer_pixels` (with `_find_layer_by_selector`).
fn mdp2_replace(ctx: &mut WriteCtx, raw: &[u8], selector: &str, png_bytes: &[u8]) -> Result<Vec<u8>> {
    let doc = tmi_clip::mdp2::parse(raw).map_err(value_err)?;
    if std::str::from_utf8(&raw[20..(20 + doc.xml_size as usize).min(raw.len())]).is_err() {
        return Err(fwd("MDP2 XML that is not UTF-8"));
    }
    let index = find_mdp2_layer(&doc, selector)?.ok_or_else(|| value_err(format!("MDP2 layer not found: {selector}")))?;
    let layer = &doc.layers[index];
    let bin = layer.bin_name.as_deref().filter(|b| !b.is_empty());
    if !bin.is_some_and(|b| doc.blob_by_name.contains_key(b)) {
        return Err(value_err(format!("MDP2 layer has no writable pixel blob: {selector}")));
    }
    if !tmi_clip::mdp2_write::writable(&layer.kind) {
        let kind = crate::pix::py_repr(&layer.kind).map_err(|_| fwd("repr of a layer type"))?;
        return Err(value_err(format!("MDP2 write-back not supported for layer type {kind}: {selector}")));
    }
    if !layer.big.is_empty() {
        return Err(fwd("MDP2 layer fields past i64"));
    }
    // the layer-sized image is made whole: its declared size is held to the limit
    ctx.limits.check_layered(layer.width, layer.height).map_err(value_err)?;
    let image = pix::pixels_from_bytes(png_bytes, &ctx.limits)?;
    let (w, h) = image.size();
    if w as i64 > layer.width || h as i64 > layer.height {
        return Err(value_err(format!(
            "replacement image {w}x{h} is larger than layer {}x{}; stage the whole file instead",
            layer.width, layer.height
        )));
    }
    let rgba = image.into_dense();
    let out = tmi_clip::mdp2_write::replace_layer_pixels(&doc, raw, index, &rgba.data, w, h).map_err(value_err)?;
    Ok(out.unwrap_or_else(|| raw.to_vec()))
}

/// mdp2.py `_find_layer_by_selector`: the layer's index.
pub fn find_mdp2_layer(doc: &tmi_clip::mdp2::Mdp2, selector: &str) -> Result<Option<usize>> {
    let sel = selector.trim_matches(is_py_space);
    if sel.is_empty() {
        return Ok(None);
    }
    let body = match sel.get(..5) {
        Some(p) if p.eq_ignore_ascii_case("mdp2:") => &sel[5..],
        _ => sel,
    };
    if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
        let Ok(id) = body.parse::<i64>() else { return Err(fwd("an MDP2 id past i64")) };
        if let Some(i) = doc.layers.iter().position(|l| l.id_key == tmi_clip::mdp2::IntKey::small(id)) {
            return Ok(Some(i));
        }
    }
    let Some(normalized) = mdp2_norm(sel)? else { return Ok(None) };
    let mut matches = Vec::new();
    for (i, layer) in doc.layers.iter().enumerate() {
        if mdp2_norm(&layer.name)?.as_deref() == Some(normalized.as_str()) {
            matches.push(i);
        }
    }
    if matches.len() > 1 {
        return Err(value_err(format!(
            "MDP2 layer selector {} is ambiguous: {} layers share that name. Rename the layers so each name is unique, \
             or select by mdp2:<id>.",
            crate::pix::py_repr(selector)?,
            matches.len()
        )));
    }
    Ok(matches.first().copied())
}

// ─────────────── PNG / JPEG (image_ops.encode_raster_like) ───────────────

#[derive(Default)]
struct SourceInfo {
    /// `Image.open(original).format`: "PNG", "JPEG" or "MPO".
    format: &'static str,
    mode: String,
    palette: Option<Vec<u8>>,
    transparency: Option<Transparency>,
    dpi: Option<(PyNum, PyNum)>,
    /// Some(None): present but None (a damaged profile).
    icc_profile: Option<Option<Vec<u8>>>,
    exif: Option<Vec<u8>>,
    comment: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    progressive: bool,
    adobe_transform: Option<u8>,
    chunks: Vec<(Vec<u8>, Vec<u8>)>,
    quantization: Vec<(u8, [u16; 64])>,
    sampling: i32,
}

/// Where raster.py's `png_header` raises `_Unsure` (image_ops then asks
/// Pillow, `_pil_source_info`).
struct Unsure;

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

/// `_safe_zlib_decompress` of an iCCP profile: Ok(None) for damaged data.
fn inflate_profile(data: &[u8]) -> std::result::Result<Option<Vec<u8>>, Unsure> {
    let limit = 1usize << 20;
    let mut d = Decompress::new(true);
    let mut out = Vec::with_capacity(64 * 1024);
    loop {
        let consumed = d.total_in() as usize;
        let room = limit - out.len();
        let mut buf = vec![0u8; room.min(64 * 1024)];
        let before_out = d.total_out();
        let status = match d.decompress(&data[consumed.min(data.len())..], &mut buf, FlushDecompress::None) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let produced = (d.total_out() - before_out) as usize;
        out.extend_from_slice(&buf[..produced]);
        if status == Status::StreamEnd {
            return Ok(Some(out));
        }
        if out.len() >= limit {
            if (d.total_in() as usize) < data.len() {
                return Err(Unsure); // "iCCP too large"
            }
            return Ok(Some(out));
        }
        if produced == 0 && d.total_in() as usize == consumed {
            return Ok(Some(out)); // input exhausted: a truncated stream gives what it has
        }
    }
}

/// `_simple_palette.match(s)`.
fn simple_palette(s: &[u8]) -> bool {
    let full = |t: &[u8]| t.iter().filter(|&&b| b == 0).count() == 1 && t.iter().all(|&b| b == 0 || b == 255);
    full(s) || (s.last() == Some(&b'\n') && full(&s[..s.len() - 1]))
}

/// raster.py `png_chunks(data)` as `png_header(after_idat=False)` walks it:
/// the chunks before the image data, and the first image data chunk whole.
fn raster_chunks_before(data: &[u8]) -> std::result::Result<Vec<(&[u8], &[u8])>, Unsure> {
    let mut out = Vec::new();
    let mut pos = 8usize;
    let n = data.len();
    loop {
        if pos + 8 > n {
            return Err(Unsure); // truncated
        }
        let length = be32(data, pos) as usize;
        let kind = &data[pos + 4..pos + 8];
        if !kind.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(Unsure); // broken chunk
        }
        if kind == b"IEND" {
            return Ok(out);
        }
        let end = (pos + 8).saturating_add(length);
        if end > n {
            return Err(Unsure); // truncated chunk
        }
        if kind == b"IDAT" || kind == b"fdAT" {
            return Ok(out);
        }
        out.push((kind, &data[pos + 8..end]));
        pos = end + 4;
    }
}

/// raster.py `png_header(original, after_idat=False)` and the rest of
/// `_source_info`'s PNG branch.
fn png_source_raster(data: &[u8]) -> std::result::Result<SourceInfo, Unsure> {
    let mut info = SourceInfo { format: "PNG", sampling: -1, ..Default::default() };
    let mut mode: Option<&'static str> = None;
    let mut size = false;
    for (kind, body) in raster_chunks_before(data)? {
        match kind {
            b"IHDR" => {
                if body.len() < 13 || body[11] != 0 {
                    return Err(Unsure);
                }
                size = true;
                mode = tmi_image::png_dec::pillow_mode(body[8], body[9]);
            }
            b"PLTE" if mode == Some("P") => info.palette = Some(body.to_vec()),
            b"tRNS" => match mode {
                Some("P") => {
                    if simple_palette(body) {
                        if let Some(i) = body.iter().position(|&b| b == 0) {
                            info.transparency = Some(Transparency::Int(i as i64));
                        }
                    } else {
                        info.transparency = Some(Transparency::Bytes(body.to_vec()));
                    }
                }
                Some(m @ ("1" | "L" | "I;16" | "RGB")) => {
                    let need = if m == "RGB" { 6 } else { 2 };
                    if body.len() < need {
                        return Err(Unsure);
                    }
                    info.transparency = Some(match m {
                        "1" => Transparency::Int(if be16(body, 0) != 0 { 255 } else { 0 }),
                        "RGB" => Transparency::Rgb(be16(body, 0) as i64, be16(body, 2) as i64, be16(body, 4) as i64),
                        _ => Transparency::Int(be16(body, 0) as i64),
                    });
                }
                _ => {}
            },
            b"gAMA" if body.len() < 4 => return Err(Unsure),
            b"cHRM" if body.len() % 4 != 0 => return Err(Unsure),
            b"sRGB" if body.is_empty() => return Err(Unsure),
            b"pHYs" => {
                if body.len() < 9 {
                    return Err(Unsure);
                }
                if body[8] == 1 {
                    let d = |v: u32| PyNum::Float(v as f64 * 0.0254);
                    info.dpi = Some((d(be32(body, 0)), d(be32(body, 4))));
                }
            }
            b"iCCP" => {
                let Some(i) = body.iter().position(|&b| b == 0) else { return Err(Unsure) };
                if i + 1 >= body.len() || body[i + 1] != 0 {
                    return Err(Unsure);
                }
                info.icc_profile = Some(inflate_profile(&body[i + 2..])?);
            }
            b"eXIf" => info.exif = Some([&b"Exif\x00\x00"[..], body].concat()),
            _ => {}
        }
    }
    if !size {
        return Err(Unsure); // no IHDR
    }
    info.mode = mode.ok_or(Unsure)?.to_string();
    Ok(info)
}

/// `_pil_source_info` of a PNG: `Image.open`'s `info` before the image data
/// (None when it cannot be opened). A palette image's `getpalette()` loads
/// it: an original whose image data fails to decode is one tmiki cannot read
/// (an opaque blob), so nothing of it is kept (None), as in image_ops.
fn png_source_pillow(data: &[u8]) -> Result<Option<SourceInfo>> {
    let Ok(scan) = tmi_image::png_dec::scan(data) else { return Ok(None) };
    let i = &scan.info;
    let mode = i.mode.expect("scan checks the mode");
    if mode == "P" && tmi_image::png_dec::decode_png(data).is_err() {
        return Ok(None); // tmiki cannot read this original: no metadata to keep
    }
    Ok(Some(SourceInfo {
        format: "PNG",
        mode: mode.to_string(),
        palette: if mode == "P" { Some(i.palette.clone().unwrap_or_default()) } else { None },
        transparency: i.transparency.clone(),
        dpi: i.dpi.map(|(x, y)| (PyNum::Float(x), PyNum::Float(y))),
        icc_profile: i.icc_profile.clone(),
        exif: i.exif.clone(),
        sampling: -1,
        ..Default::default()
    }))
}

/// image_ops `_png_ancillary_chunks(original)`: the copied chunks of the
/// whole file.
fn png_ancillary_chunks(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        let length = be32(data, pos) as usize;
        let kind = &data[pos + 4..pos + 8];
        if kind == b"IEND" {
            break;
        }
        if matches!(kind, b"gAMA" | b"cHRM" | b"sRGB" | b"cICP" | b"tEXt" | b"zTXt" | b"iTXt") {
            let end = (pos + 8).saturating_add(length).min(data.len());
            out.push((kind.to_vec(), data[pos + 8..end].to_vec()));
        }
        pos = pos.saturating_add(12).saturating_add(length);
    }
    out
}

/// `Image.open(jpeg)`'s view (`_source_info`'s JPEG branch, whether
/// tmi.raster or Pillow reads it: they agree): None when Pillow cannot open
/// the file. An MPO keeps no quantisation tables (`_pil_source_info` takes
/// them from a "JPEG" only).
fn jpeg_source(data: &[u8]) -> Option<SourceInfo> {
    let h = tmi_import::header::jpeg_open(data).ok()?;
    let mut info = SourceInfo {
        format: if h.mpo { "MPO" } else { "JPEG" },
        mode: h.mode.to_string(),
        dpi: h.dpi,
        icc_profile: h.icc_profile,
        exif: h.exif,
        comment: h.comment,
        xmp: h.xmp,
        progressive: h.progressive,
        adobe_transform: h.adobe_transform,
        sampling: -1,
        ..Default::default()
    };
    if h.mpo {
        return Some(info);
    }
    info.quantization = {
        // dict(tables): later ids replace earlier ones
        let mut q: Vec<(u8, [u16; 64])> = Vec::new();
        for (id, t) in tmi_image::jpeg::quantization(data) {
            q.retain(|x| x.0 != id);
            q.push((id, t));
        }
        q
    };
    let layers = &h.layers;
    info.sampling = if h.layer_count == 1 || h.layer_count == 4 || layers.len() < 3 {
        -1
    } else {
        match [layers[0].1, layers[0].2, layers[1].1, layers[1].2, layers[2].1, layers[2].2] {
            [1, 1, 1, 1, 1, 1] => 0,
            [2, 1, 1, 1, 1, 1] => 1,
            [2, 2, 1, 1, 1, 1] => 2,
            _ => -1,
        }
    };
    Some(info)
}

/// image_ops `_source_info(original, jpeg=...)`: None where Python gets
/// None (the original is not a PNG or JPEG tmiki reads: another format, a
/// malformed MPO, a palette PNG that does not load), and the write then keeps
/// no metadata. There is no Pillow in this decision (PLUGINS.md P0e).
fn source_info(original: &[u8], jpeg: bool) -> Result<Option<SourceInfo>> {
    if original.starts_with(pix::PNG_SIGNATURE) && !jpeg {
        let info = match png_source_raster(original) {
            Ok(info) => Some(info),
            Err(Unsure) => png_source_pillow(original)?,
        };
        return Ok(info.map(|mut i| {
            i.chunks = png_ancillary_chunks(original);
            i
        }));
    }
    if original.len() >= 3 && original[..3] == [0xFF, 0xD8, 0xFF] {
        if tmi_import::header::jpeg_open(original).is_ok_and(|h| h.malformed_mpo) {
            return Ok(None); // not an image to tmiki (an opaque blob): no metadata to keep
        }
        return Ok(jpeg_source(original));
    }
    if original.starts_with(pix::PNG_SIGNATURE) {
        return png_as_jpeg_source(original);
    }
    // Any other format (a GIF in a .png, say) is not a PNG or JPEG to take
    // metadata from.
    Ok(None)
}

/// `_pil_source_info` of a PNG written back as a JPEG (a .jpg holding PNG
/// data): Pillow's `info` after `open`, text chunks included (a tEXt
/// "comment" becomes the JPEG's comment). Values Python cannot use (a text
/// "dpi", a str where bytes go) fail there: Python reports them.
fn png_as_jpeg_source(data: &[u8]) -> Result<Option<SourceInfo>> {
    use tmi_image::png_dec::InfoVal;
    let Ok(opened) = tmi_image::png_dec::scan_open(data) else { return Ok(None) };
    let (info, loads) = match opened {
        Ok(scan) => (scan.info, true),
        Err(info) => (info, false), // opens, but load() raises
    };
    let mode = info.mode.expect("scan checks the mode");
    if mode == "P" && (!loads || tmi_image::png_dec::decode_png(data).is_err()) {
        return Ok(None); // tmiki cannot read this original: no metadata to keep
    }
    let last = |key: &str| info.info_log.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
    let bad = || fwd("a text chunk value Python cannot write into a JPEG");
    // `_bytes_or_none(value)`: bytes(value) if value else None
    let bytes_or_none = |v: Option<InfoVal>| -> Result<Option<Vec<u8>>> {
        match v {
            None => Ok(None),
            Some(InfoVal::Bytes(b)) => Ok(Some(b)),
            Some(InfoVal::Str(s)) if s.is_empty() => Ok(None),
            Some(_) => Err(bad()),
        }
    };
    let dpi = match last("dpi") {
        Some(InfoVal::Dpi(x, y)) => Some((PyNum::Float(x), PyNum::Float(y))),
        Some(InfoVal::Str(s)) if s.is_empty() => None,
        None => None,
        Some(_) => return Err(bad()), // round() of a str
    };
    let icc_profile = match last("icc_profile") {
        Some(InfoVal::Icc(p)) => Some(p),
        Some(InfoVal::Str(s)) if s.is_empty() => None,
        None => None,
        Some(_) => return Err(bad()),
    };
    let comment = match last("comment") {
        Some(InfoVal::Str(s)) => Some(s.into_bytes()), // comment.encode()
        Some(InfoVal::Bytes(b)) => Some(b),
        _ => None,
    };
    let truthy = |v: Option<InfoVal>| match v {
        Some(InfoVal::Str(s)) => !s.is_empty(),
        Some(InfoVal::Bytes(b)) => !b.is_empty(),
        Some(_) => true,
        None => false,
    };
    Ok(Some(SourceInfo {
        format: "PNG",
        mode: mode.to_string(),
        dpi,
        icc_profile,
        exif: bytes_or_none(last("exif"))?,
        xmp: bytes_or_none(last("xmp"))?,
        comment,
        progressive: truthy(last("progressive")) || truthy(last("progression")),
        sampling: -1,
        ..Default::default()
    }))
}

/// image_ops `encode_raster_like(image, original, "jpeg" | "png")`.
pub fn encode_raster_like(rgba: &tmi_import::Rgba, original: Option<&[u8]>, jpeg: bool) -> Result<Vec<u8>> {
    let img = Image::new(Mode::RGBA, rgba.width, rgba.height, rgba.data.clone());
    let Some(original) = original else {
        return if jpeg {
            write_jpeg(&img, None, |o| o.quality = 95, &[], (0, 0))
        } else {
            write_png(&img, None, None, Vec::new(), Vec::new())
        };
    };
    let Some(src) = source_info(original, jpeg)? else {
        // Pillow cannot open the original: written as if there were none
        return encode_raster_like(rgba, None, jpeg);
    };
    let icc: Option<Vec<u8>> = src.icc_profile.clone().flatten().filter(|p| !p.is_empty());
    if jpeg {
        let mut markers_src = Vec::new();
        if let Some(e) = src.exif.as_ref().filter(|e| !e.is_empty()) {
            markers_src.push((0xE1u8, e.clone()));
        }
        if let Some(x) = src.xmp.as_ref().filter(|x| !x.is_empty()) {
            markers_src.push((0xE1, [&b"http://ns.adobe.com/xap/1.0/\x00"[..], x].concat()));
        }
        // round(x) of each, passed on as C ints (OverflowError past them)
        let rounded = |v: PyNum| v.round().and_then(|r| i32::try_from(r).ok()).ok_or_else(|| fwd("a dpi past a C int"));
        let dpi = match src.dpi {
            Some((a, b)) => (rounded(a)?, rounded(b)?),
            None => (0, 0),
        };
        let qtables: Option<Vec<[u16; 64]>> = if src.format == "JPEG" && !src.quantization.is_empty() {
            let q = &src.quantization;
            let tables: Vec<[u16; 64]> =
                (0..q.len()).filter_map(|k| q.iter().find(|(id, _)| *id as usize == k).map(|(_, t)| *t)).collect();
            if !(0 < tables.len() && tables.len() < 5) {
                return Err(value_err("None or too many quantization tables"));
            }
            Some(tables)
        } else {
            None
        };
        // subsampling= goes with qtables= only (quality="keep")
        let sampling = if qtables.is_some() { src.sampling } else { -1 };
        let src_mode = Mode::from_name(&src.mode).ok_or_else(|| fwd("source mode"))?;
        let adobe_rgb = src.mode == "RGB" && src.adobe_transform == Some(0);
        let comment = src.comment.clone().filter(|c| !c.is_empty());
        let progressive = src.progressive;
        return write_jpeg_full(&img, src_mode, qtables, sampling, progressive, adobe_rgb, dpi, markers_src, icc, comment);
    }
    // PNG: _png_chunks_for(info, src)
    let mut before: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut once: Vec<&[u8]> = vec![b"cHRM", b"cICP", b"gAMA", b"sBIT", b"sRGB", b"tIME"];
    if src.format == "PNG" {
        for (kind, data) in &src.chunks {
            if let Some(i) = once.iter().position(|k| *k == kind.as_slice()) {
                once.remove(i);
                before.push((kind.clone(), data.clone()));
            } else if matches!(kind.as_slice(), b"tEXt" | b"zTXt" | b"iTXt") {
                before.push((kind.clone(), data.clone()));
            }
        }
    }
    let mut after: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if let Some((x, y)) = src.dpi {
        // struct.pack(">IIB", int(x / 0.0254 + 0.5), ...): an error past a u32
        let px = |v: PyNum| {
            let t = (v.to_f64() / 0.0254 + 0.5).trunc();
            if t.is_finite() && (0.0..=u32::MAX as f64).contains(&t) { Ok(t as u32) } else { Err(fwd("a dpi past the pHYs range")) }
        };
        let mut phys = Vec::with_capacity(9);
        phys.extend_from_slice(&px(x)?.to_be_bytes());
        phys.extend_from_slice(&px(y)?.to_be_bytes());
        phys.push(1);
        after.push((b"pHYs".to_vec(), phys));
    }
    if let Some(exif) = src.exif.as_ref().filter(|e| !e.is_empty()) {
        let body = exif.strip_prefix(b"Exif\x00\x00").unwrap_or(exif);
        after.push((b"eXIf".to_vec(), body.to_vec()));
    }
    let src_mode = Mode::from_name(&src.mode).ok_or_else(|| fwd("source mode"))?;
    let palette = src.palette.clone();
    let source = tmi_image::writeback::Source { mode: src_mode, palette: palette.as_deref(), transparency: src.transparency.clone() };
    write_png(&img, Some(&source), icc.as_deref(), before, after)
}

/// tmi_accel `writeback_png(...)`.
fn write_png(
    img: &Image,
    src: Option<&tmi_image::writeback::Source>,
    icc: Option<&[u8]>,
    before: Vec<(Vec<u8>, Vec<u8>)>,
    after: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<u8>> {
    let (out, n) = tmi_image::writeback::fit_mode(img, src, false).map_err(value_err)?;
    let mut chunks = before;
    if let (Some(icc), Some(s)) = (icc, src) {
        if tmi_image::writeback::keeps_icc(icc, out.mode, s.mode) {
            chunks.retain(|(kind, _)| kind.as_slice() != b"sRGB");
            chunks.insert(0, (b"iCCP".to_vec(), tmi_image::writeback::iccp_chunk(icc)));
        }
    }
    tmi_image::png_enc::encode_png(&out, n, &chunks, &after).map_err(value_err)
}

/// tmi_accel `writeback_jpeg(data, w, h, quality=95)` (no original).
fn write_jpeg(img: &Image, src: Option<Mode>, set: impl Fn(&mut tmi_image::jpeg::JpegOptions), markers: &[(u8, Vec<u8>)], dpi: (i32, i32)) -> Result<Vec<u8>> {
    let source = src.map(|m| tmi_image::writeback::Source { mode: m, palette: None, transparency: None });
    let (out, _) = tmi_image::writeback::fit_mode(img, source.as_ref(), true).map_err(value_err)?;
    let mut opts = tmi_image::jpeg::JpegOptions { dpi, markers: markers.to_vec(), ..Default::default() };
    set(&mut opts);
    tmi_image::jpeg::encode_jpeg(&out, &opts).map_err(|e| value_err(format!("{e:?}")))
}

/// tmi_accel `writeback_jpeg(...)` with an original's parameters.
#[allow(clippy::too_many_arguments)]
fn write_jpeg_full(
    img: &Image,
    src_mode: Mode,
    qtables: Option<Vec<[u16; 64]>>,
    subsampling: i32,
    progressive: bool,
    adobe_rgb: bool,
    dpi: (i32, i32),
    mut markers: Vec<(u8, Vec<u8>)>,
    icc: Option<Vec<u8>>,
    comment: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let source = tmi_image::writeback::Source { mode: src_mode, palette: None, transparency: None };
    let (out, _) = tmi_image::writeback::fit_mode(img, Some(&source), true).map_err(value_err)?;
    if let Some(icc) = icc {
        if tmi_image::writeback::keeps_icc(&icc, out.mode, src_mode) {
            markers.extend(tmi_image::writeback::icc_segments(&icc).into_iter().map(|seg| (0xE2, seg)));
        }
    }
    if let Some(c) = comment {
        markers.push((0xFE, c));
    }
    let quality = if qtables.is_some() { -1 } else { 95 };
    let opts = tmi_image::jpeg::JpegOptions {
        quality,
        qtables,
        subsampling,
        progressive,
        keep_rgb: adobe_rgb && out.mode == Mode::RGB,
        dpi,
        markers,
        ..Default::default()
    };
    tmi_image::jpeg::encode_jpeg(&out, &opts).map_err(|e| value_err(format!("{e:?}")))
}

/// What `Px` holds, for callers.
pub fn dense_of(px: Px) -> tmi_import::Rgba {
    px.into_dense()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmi_clip::testing::{exta_payload, minimal_clip, offscreen_attribute, outer_id, raster_layer_sql};

    /// A layer whose offscreen declares 100000 x 100000 (40 GB made whole
    /// by the write-back) on a 13000 x 13000 canvas, within the 64 canvases
    /// an offscreen may be: a merge writing one pixel into it is refused on
    /// the pixel limit before anything is allocated.
    #[test]
    fn a_write_back_into_an_offscreen_declaring_ten_gigapixels_is_refused() {
        let outer = outer_id(1);
        let attr = offscreen_attribute(100_000, 100_000, 391, 391);
        let raw = minimal_clip(13000, 13000, &raster_layer_sql(2, 0, &outer, &attr), vec![exta_payload(&outer, &[])]).unwrap();
        let blob = tmi_codec::layer_blob::encode(&[255, 0, 0, 255], 1, 1, 1, 1, 0, 0).unwrap();
        let mut warns = Warnings::default();
        let mut ctx = WriteCtx { limits: Limits { layered: 178_956_970, raster: 178_956_970 }, warns: &mut warns };
        let e = replace_layer_pixels(&mut ctx, "x.clip", Some(&raw), "clip", "clip:2", &blob, false).err().expect("refused");
        assert!(format!("{e:?}").contains("maxImagePixels"), "{e:?}");
    }
}