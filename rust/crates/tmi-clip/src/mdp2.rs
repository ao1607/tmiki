//! MDP2 (MediBang / FireAlpaca `.mdp`): the container, the XML layer table
//! and the layer tile decoder (research/mdp2-format.md).
//!
//! Reproduces `parse_mdp2`, `is_renderable_raster`, `iter_layer_tiles`,
//! `_tile_to_rgba` and the two layer decodes of the Python reader
//! (`tmiref/mdp2.py`, the code tmi used before stage 2c):
//!
//! * `render_layer_rgba(apply_opacity=False)`, the layer's own pixels that
//!   the importer stores and write-back compares: every tile composited
//!   (Pillow's `alpha_composite`) in file order onto a transparent
//!   layer-sized image (`Strictness::Image`);
//! * `_render_layer_buffer`, the compositor's decode: the same pixels, but
//!   tiles are clipped to the layer before anything is allocated, so neither
//!   the layer size nor far-away tile coordinates are checked
//!   (`Strictness::Buffer`).
//!
//! Both give the pixels of the painted rectangle only (a pixel is non-zero
//! exactly when its alpha is: compositing onto transparent keeps a pixel
//! with alpha > 0 as it is and leaves the others (0, 0, 0, 0)).
//!
//! The XML goes through roxmltree with ElementTree's view of it: attribute
//! values normalised as expat does (whitespace characters to spaces,
//! references resolved), namespaced names as `{uri}name`, only elements
//! outside any namespace found by `find("Layers")` / `findall("Layer")`.
//! Integers are CPython's `int()` (`tmi_numpy::pyint`); a value beyond i64
//! (which Python would carry as a big int) is an error here.

use std::collections::HashMap;

use flate2::{Decompress, FlushDecompress, Status};
use rayon::prelude::*;
use tmi_numpy::pyint::{py_int, py_int_decimal, py_strip};
use tmi_pillow::over_pixel;

pub const MAGIC: &[u8; 8] = b"mdipack\0";
pub const PAC_HEADER_SIZE: usize = 132;
const PAC_NAME_SIZE: usize = 64;
const FIRST_HEADER: usize = 24;
const NEXT_HEADER: usize = 16;
pub const TILE: usize = 128;

#[derive(Clone, Debug)]
pub struct PacBlob {
    pub name: String,
    pub file_off: usize,
    pub record_size: u32,
    pub flag: u32,
    pub payload_size: u32,
    pub aux_size: u32,
    pub payload_off: usize,
}

impl PacBlob {
    /// `blob_payload`: `raw[payload_off:payload_off + payload_size]`, cut
    /// short by the end of the file (the last record may claim more).
    pub fn payload<'a>(&self, raw: &'a [u8]) -> &'a [u8] {
        let start = self.payload_off.min(raw.len());
        let end = self
            .payload_off
            .saturating_add(self.payload_size as usize)
            .min(raw.len());
        &raw[start..end]
    }

    /// The record's bytes as `emit_mdp2` copies them (cut short likewise).
    pub fn record<'a>(&self, raw: &'a [u8]) -> &'a [u8] {
        let start = self.file_off.min(raw.len());
        let end = self
            .file_off
            .saturating_add(self.record_size as usize)
            .min(raw.len());
        &raw[start..end]
    }
}

#[derive(Clone, Debug, Default)]
pub struct Layer {
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub mode: String,
    pub alpha: i64,
    pub layer_id: i64,
    pub parent_id: i64,
    pub bin_name: Option<String>,
    pub kind: String,
    pub visible: bool,
    pub offset_x: i64,
    pub offset_y: i64,
    pub clipping: bool,
    pub protect_alpha: bool,
    pub folder_open: bool,
    pub bin_type: Option<String>,
    pub color: Option<String>,
    pub masking: bool,
    pub masking_type: Option<i64>,
    pub draft: bool,
    pub lumi: Option<bool>,
    pub transform_offset_x: Option<i64>,
    pub transform_offset_y: Option<i64>,
    pub halftone_type: Option<String>,
    pub halftone_line: Option<i64>,
    pub text_lines: Vec<String>,
    /// ElementTree's `attrib`, in document order.
    pub attributes: Vec<(String, String)>,
    /// Indices of the layers whose `parentId` names this layer (`children`).
    pub children: Vec<usize>,
    /// `is_renderable_raster`.
    pub renderable: bool,
    /// `layer_id` / `parent_id` exactly (what `by_id` matches on).
    pub id_key: IntKey,
    pub parent_key: IntKey,
    /// The integer fields whose values do not fit an i64 (the fields above
    /// hold them clamped), as (field, decimal text).
    pub big: Vec<(&'static str, String)>,
}

#[derive(Clone, Debug)]
pub struct Mdp2 {
    pub reserved: u32,
    pub xml_size: u32,
    pub pack_size: u32,
    pub canvas_width: i64,
    pub canvas_height: i64,
    pub dpi: i64,
    pub blobs: Vec<PacBlob>,
    pub layers: Vec<Layer>,
    /// `blob_by_name`: the last blob of each name.
    pub blob_by_name: HashMap<String, usize>,
    /// Canvas fields that do not fit an i64, as (field, decimal text).
    pub big: Vec<(&'static str, String)>,
}

fn u32le(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn attr_key(a: &roxmltree::Attribute<'_, '_>) -> String {
    match a.namespace() {
        Some(ns) => format!("{{{ns}}}{}", a.name()),
        None => a.name().to_string(),
    }
}

/// An integer as Python holds it: the value clamped to i64, and its exact
/// decimal text when it does not fit (Python keeps the big int). Equal
/// exactly when the Python ints are.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct IntKey {
    pub value: i64,
    pub big: Option<String>,
}

impl IntKey {
    pub fn small(value: i64) -> IntKey {
        IntKey { value, big: None }
    }
}

/// `int(value)` (None where Python raises ValueError).
fn py_int10(value: &str) -> Option<IntKey> {
    let v = py_int(value, 10)?;
    Some(match v.to_i64() {
        Some(x) => IntKey::small(x),
        None => IntKey {
            value: if v.negative { i64::MIN } else { i64::MAX },
            big: py_int_decimal(value),
        },
    })
}

/// `int(value)`; Err where Python raises.
fn int_strict(value: &str) -> Result<IntKey, String> {
    py_int10(value).ok_or_else(|| format!("invalid literal for int() with base 10: {value:?}"))
}

/// expat applies a document type declaration's internal subset (entity
/// values parsed again where they are used, ATTLIST defaults and attribute
/// types) in ways roxmltree does not reproduce. No MDP2 writer emits one, so
/// a declaration with an internal subset or an external id is refused; a
/// bare `<!DOCTYPE name>` changes nothing and is accepted. `prolog` is the
/// (well-formed) text before the root element.
pub fn check_doctype(prolog: &str) -> Result<(), String> {
    let mut rest = prolog;
    loop {
        rest = rest
            .trim_start_matches(|c: char| c == '\u{feff}' || matches!(c, ' ' | '\t' | '\r' | '\n'));
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |x| x.1);
        } else if let Some(r) = rest.strip_prefix("<?") {
            rest = r.split_once("?>").map_or("", |x| x.1);
        } else if let Some(r) = rest.strip_prefix("<!DOCTYPE") {
            let r = r.trim_start_matches([' ', '\t', '\r', '\n']);
            let end = r.find([' ', '\t', '\r', '\n', '>', '[']).unwrap_or(r.len());
            let after = r[end..].trim_start_matches([' ', '\t', '\r', '\n']);
            match after.strip_prefix('>') {
                Some(r) => rest = r,
                None => {
                    return Err("MDP2 XML with a document type definition is not supported".into())
                }
            }
        } else {
            return Ok(());
        }
    }
}

/// How deep [`parse`] lets MDP2's XML nest (FireAlpaca writes a few levels;
/// roxmltree recurses once per level and overflows the stack at tens of
/// thousands, which ends the process).
pub const XML_MAX_DEPTH: usize = 256;

/// The index of the first `needle` in `hay` at or after `from`.
fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// Where the tag or declaration that starts at `at` (a `<` that is no
/// comment, CDATA section or processing instruction) ends: its `>` outside
/// quotes. `Some((end, subset))`: the index of the `>`, and (for a
/// declaration, `<!DOCTYPE ...`) whether a `[` outside quotes came before it
/// (an internal subset, which this scanner does not read: it has comments and
/// quotes of its own).
fn markup_end(b: &[u8], at: usize) -> Option<(usize, bool)> {
    let declaration = b.get(at + 1) == Some(&b'!');
    let mut quote = 0u8;
    for (i, &c) in b.iter().enumerate().skip(at + 1) {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else {
            match c {
                b'"' | b'\'' => quote = c,
                b'[' if declaration => return Some((i, true)),
                b'>' => return Some((i, false)),
                _ => {}
            }
        }
    }
    None
}

const XML_NOT_CLOSED: &str = "the XML is not well formed (a tag, comment or declaration is not closed)";

/// Refuses a document whose elements nest deeper than `max_depth`, reading
/// only its tags (a tokenizer, not a parser: comments, CDATA sections,
/// processing instructions, quoted attribute values and the document type
/// declaration are skipped as XML says), before roxmltree, which recurses
/// once per level, is given it. Whatever this scanner cannot read to its end
/// is refused too (the two must agree on where the tags are); one with an
/// internal subset in its document type declaration is refused with the words
/// [`check_doctype`] uses, before anything in it is parsed. Used by [`parse`]
/// and by the plugin host's `xml.parse`.
pub fn check_xml_depth(s: &str, max_depth: usize) -> Result<(), String> {
    let b = s.as_bytes();
    let (mut i, mut depth) = (0usize, 0usize);
    while let Some(lt) = b[i..].iter().position(|&c| c == b'<').map(|p| p + i) {
        let rest = &b[lt..];
        if rest.starts_with(b"<!--") {
            i = find_from(b, lt + 4, b"-->").ok_or(XML_NOT_CLOSED)? + 3;
        } else if rest.starts_with(b"<![CDATA[") {
            i = find_from(b, lt + 9, b"]]>").ok_or(XML_NOT_CLOSED)? + 3;
        } else if rest.starts_with(b"<?") {
            i = find_from(b, lt + 2, b"?>").ok_or(XML_NOT_CLOSED)? + 2;
        } else if rest.starts_with(b"<!") {
            // <!DOCTYPE ...>
            match markup_end(b, lt).ok_or(XML_NOT_CLOSED)? {
                (_, true) => return Err("MDP2 XML with a document type definition is not supported".into()),
                (e, false) => i = e + 1,
            }
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            i = find_from(b, lt + 2, b">").ok_or(XML_NOT_CLOSED)? + 1;
        } else {
            let (e, _) = markup_end(b, lt).ok_or(XML_NOT_CLOSED)?;
            if b[e - 1] != b'/' {
                depth += 1;
                if depth > max_depth {
                    return Err(format!("elements nested more than {max_depth} deep"));
                }
            }
            i = e + 1;
        }
    }
    Ok(())
}

fn is_supported_type(kind: &str) -> bool {
    matches!(kind, "32bpp" | "8bpp" | "1bpp" | "text" | "vector")
}

/// `parse_mdp2`.
pub fn parse(data: &[u8]) -> Result<Mdp2, String> {
    if data.len() < 8 || &data[..8] != MAGIC {
        return Err("not an mdipack/MDP2 file".into());
    }
    if data.len() < 20 {
        return Err("unpack requires a buffer of 12 bytes".into());
    }
    let reserved = u32le(data, 8).unwrap();
    let xml_size = u32le(data, 12).unwrap();
    let pack_size = u32le(data, 16).unwrap();
    let xml_start = 20usize;
    let pack_start = xml_start + xml_size as usize;
    if pack_start + pack_size as usize != data.len() {
        return Err(format!(
            "header sizes do not match file length: {pack_start}+{pack_size}!={}",
            data.len()
        ));
    }
    let xml_text = std::str::from_utf8(&data[xml_start..pack_start])
        .map_err(|e| format!("'utf-8' codec can't decode the XML: {e}"))?;
    // roxmltree recurses once per nesting level: a crafted file nested tens
    // of thousands deep would overflow the stack and end the process
    check_xml_depth(xml_text, XML_MAX_DEPTH).map_err(|e| format!("MDP2 XML: {e}"))?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml_text, options)
        .map_err(|e| format!("MDP2 XML: {e}"))?;
    let root = doc.root_element();
    check_doctype(&xml_text[..root.range().start])?;
    let root_attr = |key: &str| {
        root.attributes()
            .find(|a| attr_key(a) == key)
            .map(|a| a.value())
    };
    let mut doc_big = Vec::new();
    let mut root_int = |key: &'static str, value: &str| -> Result<i64, String> {
        let v = int_strict(value)?;
        if let Some(text) = v.big {
            doc_big.push((key, text));
        }
        Ok(v.value)
    };
    let canvas_width = root_int(
        "canvas_width",
        root_attr("width").ok_or("MDP2 XML has no width")?,
    )?;
    let canvas_height = root_int(
        "canvas_height",
        root_attr("height").ok_or("MDP2 XML has no height")?,
    )?;
    let dpi = root_int("dpi", root_attr("dpi").unwrap_or("0"))?;

    let mut blobs = Vec::new();
    let mut off = pack_start;
    while off < data.len() {
        if data.get(off..off + 4) != Some(b"PAC ".as_slice()) {
            return Err(format!("expected PAC at 0x{off:X}"));
        }
        let header = |k: usize| {
            u32le(data, off + 4 + 4 * k)
                .ok_or_else(|| "unpack requires a buffer of 16 bytes".to_string())
        };
        let (record_size, flag, payload_size, aux_size) =
            (header(0)?, header(1)?, header(2)?, header(3)?);
        if (record_size as usize) < PAC_HEADER_SIZE {
            return Err(format!("bad PAC record size at 0x{off:X}: {record_size}"));
        }
        let name_bytes =
            &data[(off + 68).min(data.len())..(off + 68 + PAC_NAME_SIZE).min(data.len())];
        let name_bytes = name_bytes.split(|&b| b == 0).next().unwrap_or(&[]);
        if !name_bytes.is_ascii() {
            return Err("'ascii' codec can't decode a PAC blob name".into());
        }
        let name = String::from_utf8(name_bytes.to_vec()).unwrap();
        if record_size as u64 != PAC_HEADER_SIZE as u64 + payload_size as u64 {
            return Err(format!("PAC size mismatch for {name:?}"));
        }
        blobs.push(PacBlob {
            name,
            file_off: off,
            record_size,
            flag,
            payload_size,
            aux_size,
            payload_off: off + PAC_HEADER_SIZE,
        });
        off += record_size as usize;
    }
    let mut blob_by_name = HashMap::new();
    for (i, b) in blobs.iter().enumerate() {
        blob_by_name.insert(b.name.clone(), i);
    }

    let mut layers = Vec::new();
    let in_no_ns = |n: &roxmltree::Node<'_, '_>, name: &str| {
        n.is_element() && n.tag_name().namespace().is_none() && n.tag_name().name() == name
    };
    if let Some(layers_el) = root.children().find(|n| in_no_ns(n, "Layers")) {
        for el in layers_el.children().filter(|n| in_no_ns(n, "Layer")) {
            let attributes: Vec<(String, String)> = el
                .attributes()
                .map(|a| (attr_key(&a), a.value().to_string()))
                .collect();
            let get = |key: &str| {
                attributes
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            };
            let bool_attr = |key: &str| get(key).map(|v| v.to_lowercase() == "true");
            let mut big: Vec<(&'static str, String)> = Vec::new();
            let mut keep = |field: &'static str, v: IntKey| -> i64 {
                if let Some(text) = v.big {
                    big.push((field, text));
                }
                v.value
            };
            // `_int_attr` / `_optional_int_attr`: absent or not an integer
            // gives the default / None.
            let tofsx = get("tofsx").and_then(py_int10);
            let tofsy = get("tofsy").and_then(py_int10);
            let offset_x = get("ofsx")
                .and_then(py_int10)
                .or_else(|| tofsx.clone())
                .unwrap_or_default();
            let offset_y = get("ofsy")
                .and_then(py_int10)
                .or_else(|| tofsy.clone())
                .unwrap_or_default();
            let id_key = int_strict(get("id").unwrap_or("0"))?;
            let parent_key = int_strict(get("parentId").unwrap_or("-1"))?;
            let width = int_strict(get("width").unwrap_or("0"))?;
            let height = int_strict(get("height").unwrap_or("0"))?;
            let alpha = int_strict(get("alpha").unwrap_or("255"))?;
            let masking_type = get("maskingType").and_then(py_int10);
            let halftone_line = get("halftoneLine").and_then(py_int10);
            let mut layer = Layer {
                name: get("name").unwrap_or("").to_string(),
                width: keep("width", width),
                height: keep("height", height),
                mode: get("mode").unwrap_or("normal").to_string(),
                alpha: keep("alpha", alpha),
                layer_id: keep("layer_id", id_key.clone()),
                parent_id: keep("parent_id", parent_key.clone()),
                bin_name: get("bin").map(str::to_string),
                kind: get("type").unwrap_or("").to_string(),
                visible: get("visible").unwrap_or("true") != "false",
                offset_x: keep("offset_x", offset_x),
                offset_y: keep("offset_y", offset_y),
                clipping: get("clipping").unwrap_or("false") == "true",
                protect_alpha: get("protectAlpha").unwrap_or("false") == "true",
                folder_open: get("folderOpen").unwrap_or("false") == "true",
                bin_type: get("binType").map(str::to_string),
                color: get("color").map(str::to_string),
                masking: bool_attr("masking").unwrap_or(false),
                masking_type: masking_type.map(|v| keep("masking_type", v)),
                draft: bool_attr("draft").unwrap_or(false),
                lumi: bool_attr("lumi"),
                transform_offset_x: tofsx.map(|v| keep("transform_offset_x", v)),
                transform_offset_y: tofsy.map(|v| keep("transform_offset_y", v)),
                halftone_type: get("halftoneType").map(str::to_string),
                halftone_line: halftone_line.map(|v| keep("halftone_line", v)),
                text_lines: el
                    .children()
                    .filter(|n| in_no_ns(n, "Line"))
                    .map(|line| {
                        line.attributes()
                            .find(|a| attr_key(a) == "text")
                            .map_or(String::new(), |a| a.value().to_string())
                    })
                    .collect(),
                attributes: Vec::new(),
                children: Vec::new(),
                renderable: false,
                id_key,
                parent_key,
                big: Vec::new(),
            };
            layer.big = big;
            layer.attributes = attributes;
            layers.push(layer);
        }
    }
    // by_id keeps the last layer of each id.
    let mut by_id: HashMap<&IntKey, usize> = HashMap::new();
    for (i, l) in layers.iter().enumerate() {
        by_id.insert(&l.id_key, i);
    }
    let parents: Vec<Option<usize>> = layers
        .iter()
        .map(|l| by_id.get(&l.parent_key).copied())
        .collect();
    for (i, p) in parents.into_iter().enumerate() {
        if let Some(p) = p {
            layers[p].children.push(i);
        }
    }
    let mut doc = Mdp2 {
        reserved,
        xml_size,
        pack_size,
        canvas_width,
        canvas_height,
        dpi,
        blobs,
        layers,
        blob_by_name,
        big: doc_big,
    };
    for i in 0..doc.layers.len() {
        let r = renderable(&doc, &doc.layers[i], data);
        doc.layers[i].renderable = r;
    }
    Ok(doc)
}

/// `is_renderable_raster`.
fn renderable(doc: &Mdp2, layer: &Layer, raw: &[u8]) -> bool {
    let Some(bin) = &layer.bin_name else {
        return false;
    };
    if bin.is_empty() {
        return false;
    }
    let Some(&bi) = doc.blob_by_name.get(bin) else {
        return false;
    };
    let blob = &doc.blobs[bi];
    if blob.payload_size < 4 {
        return false;
    }
    let payload = blob.payload(raw);
    if is_supported_type(&layer.kind) && blob.payload_size == 4 {
        return u32le(payload, 0) == Some(0);
    }
    if blob.payload_size as usize <= FIRST_HEADER {
        return false;
    }
    match (
        u32le(payload, 0),
        u32le(payload, 4),
        payload.len() >= FIRST_HEADER,
    ) {
        (Some(count), Some(tile_size), true) => {
            tile_size == TILE as u32 && count > 0 && is_supported_type(&layer.kind)
        }
        _ => false,
    }
}

/// One tile record: its cell and the bytes `zlib.decompress` is given (the
/// declared size, cut short by the payload's end).
#[derive(Clone, Copy)]
pub struct TileRec<'a> {
    pub x: u32,
    pub y: u32,
    pub data: &'a [u8],
}

/// `iter_layer_tiles` without the decompression: the records, or the error
/// Python raises (before or after yielding tiles: a render fails either
/// way). `payload_size` is the PAC header's.
pub fn scan_tiles(payload: &[u8], payload_size: u32) -> Result<Vec<TileRec<'_>>, String> {
    if payload_size <= 4 {
        return Ok(Vec::new());
    }
    let short = || "unpack requires a buffer of 24 bytes".to_string();
    if payload.len() < FIRST_HEADER {
        return Err(short());
    }
    let count = u32le(payload, 0).unwrap();
    let tile_size = u32le(payload, 4).unwrap();
    if tile_size != TILE as u32 {
        return Err(format!("unsupported tile size {tile_size}"));
    }
    let (mut x, mut y) = (u32le(payload, 8).unwrap(), u32le(payload, 12).unwrap());
    let mut size = u32le(payload, 20).unwrap() as usize;
    let mut pos = FIRST_HEADER;
    let mut tiles = Vec::with_capacity((count as usize).min(payload.len() / NEXT_HEADER + 1));
    for index in 0..count {
        let start = pos.min(payload.len());
        let end = pos.saturating_add(size).min(payload.len());
        tiles.push(TileRec {
            x,
            y,
            data: &payload[start..end],
        });
        pos = pos.saturating_add(size);
        if index != count - 1 {
            let pad = pos.wrapping_neg() % 4;
            let p0 = pos.min(payload.len());
            if payload[p0..pos.saturating_add(pad).min(payload.len())]
                .iter()
                .any(|&b| b != 0)
            {
                return Err(format!("nonzero tile padding at payload offset {pos}"));
            }
            pos = pos.saturating_add(pad);
            let field = |k: usize| {
                u32le(payload, pos.saturating_add(4 * k))
                    .ok_or_else(|| "unpack requires a buffer of 16 bytes".to_string())
            };
            let (nx, ny, zero, nsize) = (field(0)?, field(1)?, field(2)?, field(3)?);
            if zero != 0 {
                return Err(format!("unexpected tile header word {zero}"));
            }
            x = nx;
            y = ny;
            size = nsize as usize;
            pos += NEXT_HEADER;
        }
    }
    let pad = pos.wrapping_neg() % 4;
    if pad != 0
        && pos.saturating_add(pad) == payload.len()
        && payload[pos..].iter().all(|&b| b == 0)
    {
        pos += pad;
    }
    if pos != payload.len() {
        return Err(format!("trailing payload bytes at {pos}/{}", payload.len()));
    }
    Ok(tiles)
}

/// How a layer's tiles turn into RGBA (`_tile_to_rgba`).
#[derive(Clone, Copy, Debug)]
pub enum TileFormat {
    /// 32bpp / text / vector: BGRA.
    Bgra,
    /// 8bpp: one byte per pixel, tinted by the colour.
    Gray([u8; 4]),
    /// 1bpp: MSB-first bits (0 or 255), tinted by the colour.
    Mono([u8; 4]),
}

impl TileFormat {
    pub fn for_layer(layer: &Layer) -> Result<TileFormat, String> {
        match layer.kind.as_str() {
            "32bpp" | "text" | "vector" => Ok(TileFormat::Bgra),
            "8bpp" => Ok(TileFormat::Gray(parse_argb(layer.color.as_deref()))),
            "1bpp" => Ok(TileFormat::Mono(parse_argb(layer.color.as_deref()))),
            other => Err(format!("unsupported layer type {other:?}")),
        }
    }

    pub fn tile_bytes(self) -> usize {
        match self {
            TileFormat::Bgra => TILE * TILE * 4,
            TileFormat::Gray(_) => TILE * TILE,
            TileFormat::Mono(_) => TILE * TILE / 8,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TileFormat::Bgra => "32bpp",
            TileFormat::Gray(_) => "8bpp",
            TileFormat::Mono(_) => "1bpp",
        }
    }

    /// Pixel `i` of an inflated tile as RGBA.
    #[inline]
    pub fn pixel(self, data: &[u8], i: usize) -> [u8; 4] {
        match self {
            TileFormat::Bgra => [
                data[i * 4 + 2],
                data[i * 4 + 1],
                data[i * 4],
                data[i * 4 + 3],
            ],
            TileFormat::Gray(c) => [c[0], c[1], c[2], tint(data[i], c[3])],
            TileFormat::Mono(c) => {
                let bit = (data[i / 8] >> (7 - (i % 8))) & 1;
                [c[0], c[1], c[2], tint(if bit != 0 { 255 } else { 0 }, c[3])]
            }
        }
    }
}

/// `_tinted_alpha_image`'s `alpha.point(lambda v: v * ca // 255)`.
#[inline]
fn tint(v: u8, ca: u8) -> u8 {
    if ca == 255 {
        v
    } else {
        (v as u32 * ca as u32 / 255) as u8
    }
}

/// `_parse_argb`: (r, g, b, a) of an "AARRGGBB" / "RRGGBB" hex colour.
pub fn parse_argb(value: Option<&str>) -> [u8; 4] {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return [0, 0, 0, 255];
    };
    let text = py_strip(value);
    let text = text.strip_prefix('#').unwrap_or(text);
    let Some(raw) = py_int(text, 16) else {
        return [0, 0, 0, 255];
    };
    let bits = raw.low_bits();
    let byte = |shift: u32| ((bits >> shift) & 0xff) as u8;
    if text.chars().count() <= 6 {
        [byte(16), byte(8), byte(0), 255]
    } else {
        [byte(16), byte(8), byte(0), byte(24)]
    }
}

/// `zlib.decompress(data)` of one tile: the first zlib stream in `data`
/// (bytes after its end are ignored, as Python does), which must inflate
/// to exactly `expected` bytes.
pub fn inflate_tile(data: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected + 1);
    let mut d = Decompress::new(true);
    match d.decompress_vec(data, &mut out, FlushDecompress::Finish) {
        Ok(Status::StreamEnd) if out.len() == expected => Ok(out),
        Ok(Status::StreamEnd) => Err(format!("got {} bytes", out.len())),
        Ok(_) if out.len() > expected => Err("got more bytes".into()),
        Ok(_) => Err("Error -5 while decompressing data: incomplete or truncated stream".into()),
        Err(e) => Err(format!("Error while decompressing data: {e}")),
    }
}

/// What a layer decode checks beyond the tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    /// `render_layer_rgba`: Pillow builds the layer-sized image (a negative
    /// or > 2^31 size raises) and composites each tile through `crop` /
    /// `paste`, whose C ints overflow for a tile ending past 2^31 - 1.
    Image,
    /// `_render_layer_buffer`: tiles are clipped to the layer first.
    Buffer,
}

/// A decoded layer: its painted rectangle (layer coordinates) and that
/// rectangle's RGBA, or nothing painted.
#[derive(Clone, Debug, Default)]
pub struct LayerPixels {
    pub rect: Option<(usize, usize, usize, usize)>,
    pub rgba: Vec<u8>,
}

impl LayerPixels {
    pub fn width(&self) -> usize {
        self.rect.map_or(0, |r| r.2 - r.0)
    }
}

/// The layer size `render_layer_rgba` gives an image of: `(max(w, 1),
/// max(h, 1))` for a layer that is not renderable.
pub fn image_size(layer: &Layer) -> Result<(usize, usize), String> {
    if !layer.renderable {
        return Ok((layer.width.max(1) as usize, layer.height.max(1) as usize));
    }
    if layer.width < 0 || layer.height < 0 {
        return Err("Width and height must be >= 0".into());
    }
    if layer.width > i32::MAX as i64 || layer.height > i32::MAX as i64 {
        return Err("Python int too large to convert to C int".into());
    }
    Ok((layer.width as usize, layer.height as usize))
}

#[derive(Clone, Copy)]
struct OutPtr(*mut u8);
unsafe impl Send for OutPtr {}
unsafe impl Sync for OutPtr {}

/// A layer decode in two steps: `plan_layer` reads and checks every tile and
/// finds the painted rectangle, `fill` writes the rectangle's pixels (into a
/// buffer the caller owns, e.g. a Python bytes object).
pub struct LayerPlan<'a> {
    tiles: Vec<TileRec<'a>>,
    painted: Vec<bool>,
    format: TileFormat,
    /// The painted rectangle in layer coordinates (None: nothing painted).
    pub rect: Option<(usize, usize, usize, usize)>,
}

/// The layer's own pixels (no opacity). The layer must be renderable.
pub fn decode_layer(
    doc: &Mdp2,
    raw: &[u8],
    index: usize,
    strictness: Strictness,
) -> Result<LayerPixels, String> {
    decode_layer_limited(doc, raw, index, strictness, None)
}

/// The most pixels a decoded layer rectangle may hold when the caller names
/// no limit (`core.maxImagePixels` is the caller's to pass): 2^30, 4 GiB of
/// RGBA. A hostile file's tiles can paint a rectangle of 2^31 x 2^31.
pub const MAX_RECT_PIXELS: u64 = 1 << 30;

/// `decode_layer` held to `max_pixels` (`None`: `MAX_RECT_PIXELS`; 0: no limit
/// beyond what a `Vec` can hold) before the rectangle is allocated.
pub fn decode_layer_limited(
    doc: &Mdp2,
    raw: &[u8],
    index: usize,
    strictness: Strictness,
    max_pixels: Option<u64>,
) -> Result<LayerPixels, String> {
    let plan = plan_layer(doc, raw, index, strictness)?;
    let Some(rect) = plan.rect else {
        return Ok(LayerPixels::default());
    };
    let len = plan.rgba_len(max_pixels)?;
    let mut rgba = vec![0u8; len];
    plan.fill(&mut rgba);
    Ok(LayerPixels {
        rect: Some(rect),
        rgba,
    })
}

/// Pass 1 of `decode_layer`.
pub fn plan_layer<'a>(
    doc: &Mdp2,
    raw: &'a [u8],
    index: usize,
    strictness: Strictness,
) -> Result<LayerPlan<'a>, String> {
    let layer = &doc.layers[index];
    debug_assert!(layer.renderable);
    let bin = layer.bin_name.as_deref().unwrap_or("");
    let blob = &doc.blobs[doc.blob_by_name[bin]];
    let what = |e: String| format!("{bin}: {e}");
    let (lw, lh): (i64, i64) = match strictness {
        Strictness::Image => {
            let (w, h) = image_size(layer)?;
            (w as i64, h as i64)
        }
        Strictness::Buffer => (layer.width, layer.height),
    };
    let tiles = scan_tiles(blob.payload(raw), blob.payload_size).map_err(what)?;
    if tiles.is_empty() {
        return Ok(LayerPlan {
            tiles,
            painted: Vec::new(),
            format: TileFormat::Bgra,
            rect: None,
        });
    }
    let format = TileFormat::for_layer(layer).map_err(what)?;
    if strictness == Strictness::Image {
        let limit = i32::MAX as u64;
        if tiles.iter().any(|t| {
            t.x as u64 * TILE as u64 + TILE as u64 > limit
                || t.y as u64 * TILE as u64 + TILE as u64 > limit
        }) {
            return Err("signed integer is greater than maximum".into());
        }
    }
    let expected = format.tile_bytes();
    // Pass 1: inflate every tile (each must decode, wherever it lies) and
    // find its painted part inside the layer.
    let boxes: Vec<Result<Option<(usize, usize, usize, usize)>, String>> = tiles
        .par_iter()
        .enumerate()
        .map(|(index, t)| {
            let data = inflate_tile(t.data, expected).map_err(|e| {
                format!("{bin} tile {index}: expected {} data: {e}", format.label())
            })?;
            let (px, py) = (t.x as i64 * TILE as i64, t.y as i64 * TILE as i64);
            let (cw, ch) = (
                lw.saturating_sub(px).clamp(0, TILE as i64) as usize,
                lh.saturating_sub(py).clamp(0, TILE as i64) as usize,
            );
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
            for ly in 0..ch {
                let (first, last) = match format {
                    TileFormat::Bgra => {
                        let row = &data[ly * TILE * 4..ly * TILE * 4 + cw * 4];
                        (
                            row.chunks_exact(4).position(|p| p[3] != 0),
                            row.chunks_exact(4).rposition(|p| p[3] != 0),
                        )
                    }
                    _ => (
                        (0..cw).find(|&lx| format.pixel(&data, ly * TILE + lx)[3] != 0),
                        (0..cw)
                            .rev()
                            .find(|&lx| format.pixel(&data, ly * TILE + lx)[3] != 0),
                    ),
                };
                if let (Some(a), Some(b)) = (first, last) {
                    x0 = x0.min(a);
                    x1 = x1.max(b + 1);
                    y0 = y0.min(ly);
                    y1 = ly + 1;
                }
            }
            Ok((x0 != usize::MAX).then(|| {
                (
                    px as usize + x0,
                    py as usize + y0,
                    px as usize + x1,
                    py as usize + y1,
                )
            }))
        })
        .collect();
    let mut rect: Option<(usize, usize, usize, usize)> = None;
    let mut painted = vec![false; tiles.len()];
    for (i, b) in boxes.into_iter().enumerate() {
        if let Some(b) = b? {
            painted[i] = true;
            rect = Some(match rect {
                None => b,
                Some(r) => (r.0.min(b.0), r.1.min(b.1), r.2.max(b.2), r.3.max(b.3)),
            });
        }
    }
    Ok(LayerPlan {
        tiles,
        painted,
        format,
        rect,
    })
}

impl LayerPlan<'_> {
    /// The byte length of the painted rectangle's RGBA (0: nothing painted),
    /// every product checked. Over `max_pixels` (`None`: `MAX_RECT_PIXELS`; 0:
    /// no limit) is an error naming the rectangle.
    pub fn rgba_len(&self, max_pixels: Option<u64>) -> Result<usize, String> {
        let Some(rect) = self.rect else { return Ok(0) };
        let (w, h) = (rect.2 - rect.0, rect.3 - rect.1);
        let too_large = || format!("layer is too large: its painted area is {w}x{h} pixels, over the limit");
        let pixels = (w as u64).checked_mul(h as u64).ok_or_else(too_large)?;
        let limit = max_pixels.unwrap_or(MAX_RECT_PIXELS);
        if limit != 0 && pixels > limit {
            return Err(too_large());
        }
        usize::try_from(pixels)
            .ok()
            .and_then(|p| p.checked_mul(4))
            .filter(|&b| b <= isize::MAX as usize)
            .ok_or_else(too_large)
    }

    /// Pass 2: composite the painted tiles into `out`, the rectangle's RGBA
    /// (zero-filled). Tiles of different cells touch different pixels; the
    /// tiles of one cell go in file order.
    pub fn fill(&self, out: &mut [u8]) {
        let Some(rect) = self.rect else { return };
        let (tiles, painted, format) = (&self.tiles, &self.painted, self.format);
        let expected = format.tile_bytes();
        let rw = rect.2 - rect.0;
        assert_eq!(out.len(), rw * (rect.3 - rect.1) * 4);
        let mut cells: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
        for (i, t) in tiles.iter().enumerate() {
            if painted[i] {
                cells.entry((t.x, t.y)).or_default().push(i);
            }
        }
        let cells: Vec<Vec<usize>> = cells.into_values().collect();
        let ptr = OutPtr(out.as_mut_ptr());
        cells.par_iter().for_each(|members| {
            let ptr = ptr;
            for (n, &i) in members.iter().enumerate() {
                let t = tiles[i];
                let data = inflate_tile(t.data, expected).expect("inflated in pass 1");
                let (px, py) = (t.x as usize * TILE, t.y as usize * TILE);
                let cx0 = px.max(rect.0);
                let cx1 = (px + TILE).min(rect.2);
                let cy0 = py.max(rect.1);
                let cy1 = (py + TILE).min(rect.3);
                for y in cy0..cy1 {
                    let off = ((y - rect.1) * rw + (cx0 - rect.0)) * 4;
                    // SAFETY: rows [cy0, cy1) x columns [cx0, cx1) lie inside the
                    // rectangle and belong to this cell only.
                    let dst =
                        unsafe { std::slice::from_raw_parts_mut(ptr.0.add(off), (cx1 - cx0) * 4) };
                    if n == 0 && matches!(format, TileFormat::Bgra) {
                        let s0 = ((y - py) * TILE + (cx0 - px)) * 4;
                        let src = &data[s0..s0 + (cx1 - cx0) * 4];
                        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                            if s[3] != 0 {
                                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                            }
                        }
                        continue;
                    }
                    for (k, x) in (cx0..cx1).enumerate() {
                        let s = format.pixel(&data, (y - py) * TILE + (x - px));
                        if n == 0 {
                            // Over a transparent pixel: the source where its
                            // alpha is not 0 (exactly what over_pixel gives).
                            if s[3] != 0 {
                                dst[k * 4..k * 4 + 4].copy_from_slice(&s);
                            }
                        } else {
                            over_pixel(&mut dst[k * 4..k * 4 + 4], &s);
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `.mdp` whose XML is `xml` and whose pack is empty.
    fn mdp_with_xml(xml: &str) -> Vec<u8> {
        let mut data = MAGIC.to_vec();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(xml.as_bytes());
        data
    }

    #[test]
    fn deeply_nested_xml_is_refused_before_it_is_parsed() {
        // the security review's file: 50,000 levels overflowed a 16 MiB stack
        // in roxmltree and ended the process (STATUS_STACK_OVERFLOW). Run on
        // a 1 MiB stack: the refusal must come before any recursion.
        let deep = format!("{}{}", "<a>".repeat(50_000), "</a>".repeat(50_000));
        let data = mdp_with_xml(&deep);
        let err = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || parse(&data).err())
            .unwrap()
            .join()
            .expect("parse must not overflow the stack")
            .expect("a 50,000-deep document must be refused");
        assert!(err.contains("nested more than 256 deep"), "{err}");
        // the limit itself parses past the depth check (it fails later, on the
        // missing Mdipack attributes, not on the nesting)
        let edge = format!("{}{}", "<a>".repeat(XML_MAX_DEPTH), "</a>".repeat(XML_MAX_DEPTH));
        assert!(!parse(&mdp_with_xml(&edge)).unwrap_err().contains("nested"));
        let over = format!("{}{}", "<a>".repeat(XML_MAX_DEPTH + 1), "</a>".repeat(XML_MAX_DEPTH + 1));
        assert!(parse(&mdp_with_xml(&over)).unwrap_err().contains("nested more than 256 deep"));
        // quotes and comments do not hide tags from the scanner
        assert!(check_xml_depth("<a x='>'><!-- <b> --><c/></a>", 1).is_ok());
        assert!(check_xml_depth("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a/>", 8).unwrap_err().contains("document type definition"));
    }

    /// The audit's hostile files (`gen_mdp.py`): one 128x128 tile with a
    /// single opaque pixel, placed far out.
    fn hostile_mdp(cw: i64, ch: i64, lw: i64, lh: i64, tiles: &[(u32, u32)]) -> Vec<u8> {
        let mut raw = vec![0u8; TILE * TILE * 4];
        raw[..4].copy_from_slice(&[0, 0, 255, 255]);
        let z = {
            use std::io::Write;
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&raw).unwrap();
            e.finish().unwrap()
        };
        let mut payload = Vec::new();
        for (i, &(x, y)) in tiles.iter().enumerate() {
            while payload.len() % 4 != 0 {
                payload.push(0);
            }
            if i == 0 {
                payload.extend_from_slice(&(tiles.len() as u32).to_le_bytes());
                payload.extend_from_slice(&(TILE as u32).to_le_bytes());
            }
            for v in [x, y, 0, z.len() as u32] {
                payload.extend_from_slice(&v.to_le_bytes());
            }
            payload.extend_from_slice(&z);
        }
        while payload.len() % 4 != 0 {
            payload.push(0);
        }
        let mut pac = vec![0u8; 132];
        pac[0..4].copy_from_slice(b"PAC ");
        pac[4..8].copy_from_slice(&((132 + payload.len()) as u32).to_le_bytes());
        pac[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        pac[68..70].copy_from_slice(b"b1");
        pac.extend_from_slice(&payload);
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Mdipack width=\"{cw}\" height=\"{ch}\" dpi=\"72\"><Layers><Layer name=\"L1\" id=\"1\" parentId=\"-1\" width=\"{lw}\" height=\"{lh}\" type=\"32bpp\" bin=\"b1\" alpha=\"255\" visible=\"true\"/></Layers></Mdipack>"
        );
        let mut data = MAGIC.to_vec();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        data.extend_from_slice(&(pac.len() as u32).to_le_bytes());
        data.extend_from_slice(xml.as_bytes());
        data.extend_from_slice(&pac);
        data
    }

    #[test]
    fn a_far_tile_layer_is_refused_not_allocated() {
        let far = 16_777_214u32;
        let w = i32::MAX as i64;
        // a rectangle of 2^31 x 2^31 (its byte size wraps a usize), and one of
        // 2^31 x 1 (8 GiB): both refused by every decode path, none panics
        for (data, lh) in [(hostile_mdp(16, 16, w, w, &[(0, 0), (far, 0), (0, far)]), w), (hostile_mdp(16, 16, w, 1, &[(0, 0), (far, 0)]), 1)] {
            let doc = parse(&data).unwrap();
            assert_eq!(doc.layers[0].height, lh);
            for s in [Strictness::Buffer, Strictness::Image] {
                let err = decode_layer(&doc, &data, 0, s).unwrap_err();
                assert!(err.contains("too large"), "{err}");
            }
            let plan = plan_layer(&doc, &data, 0, Strictness::Image).unwrap();
            assert!(plan.rgba_len(None).is_err());
            assert!(plan.rgba_len(Some(1000)).is_err());
        }
        // the limit the caller names: a small normal layer decodes under it
        let ok = hostile_mdp(16, 16, 16, 16, &[(0, 0)]);
        let doc = parse(&ok).unwrap();
        // (one pixel is painted)
        assert!(decode_layer_limited(&doc, &ok, 0, Strictness::Buffer, Some(1)).is_ok());
        assert!(decode_layer(&doc, &ok, 0, Strictness::Image).is_ok());
    }

    #[test]
    fn argb_follows_python() {
        assert_eq!(parse_argb(None), [0, 0, 0, 255]);
        assert_eq!(parse_argb(Some("")), [0, 0, 0, 255]);
        assert_eq!(parse_argb(Some("FF102030")), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(parse_argb(Some("#102030")), [0x10, 0x20, 0x30, 255]);
        assert_eq!(parse_argb(Some(" 80102030 ")), [0x10, 0x20, 0x30, 0x80]);
        // "0x" counts towards the length: alpha from bits 24..32 (0 here).
        assert_eq!(parse_argb(Some("0x102030")), [0x10, 0x20, 0x30, 0]);
        assert_eq!(parse_argb(Some("zz")), [0, 0, 0, 255]);
        assert_eq!(parse_argb(Some("-1")), [255, 255, 255, 255]);
    }

    #[test]
    fn over_transparent_is_a_copy() {
        // decode_layer copies the first tile of a cell instead of compositing.
        for a in 0..=255u8 {
            for c in 0..=255u8 {
                let src = [c, 255 - c, c / 3, a];
                let mut dst = [0u8; 4];
                over_pixel(&mut dst, &src);
                assert_eq!(dst, if a == 0 { [0; 4] } else { src });
            }
        }
    }

    #[test]
    fn mono_bits_are_msb_first() {
        let mut data = vec![0u8; TILE * TILE / 8];
        data[0] = 0b1000_0001;
        let f = TileFormat::Mono([1, 2, 3, 255]);
        assert_eq!(f.pixel(&data, 0), [1, 2, 3, 255]);
        assert_eq!(f.pixel(&data, 1), [1, 2, 3, 0]);
        assert_eq!(f.pixel(&data, 7), [1, 2, 3, 255]);
    }
}
