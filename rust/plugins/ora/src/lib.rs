//! An OpenRaster (`.ora`) reader and writer for tmiki (PLUGINS.md 7.3),
//! written once on `tmi-plugin-sdk` ([`plugin::Ora`]) and built three ways:
//!
//! - for `wasm32-wasip2` it is a component of the `tmi:format/format-plugin`
//!   world (`export_plugin!` below), the shape every distributed plugin has;
//! - natively with feature `dev`, the same [`plugin::Ora`] is a development
//!   plugin (`src/bin/ora-dev.rs`, `tmiki plugin dev`: the SDK's RPC);
//! - natively the reader's core is also a plain library: `tmi-plugin-host`'s
//!   `OraNative` wraps it in a `tmi_format::FormatReader` in-process and its
//!   tests compare the records and bytes the three give (PLUGINS.md P5, P7,
//!   6.4 item 4).
//!
//! The core sees the file only through [`Host`] (the WIT `source-file` and
//! `codecs.transform`) and returns [`model`]'s plain values, which each
//! side converts to its own types one for one (`plugin.rs` to the SDK's).
//!
//! What it reads: a zip (stored or deflate members, no zip64) holding
//! `stack.xml`, the layers' PNGs and optionally `mergedimage.png`.
//! `stack.xml` is parsed inside the plugin (roxmltree; PLUGINS.md 3.3 asks
//! for the XML to be the component's own). Every layer is `ready` (the host
//! decodes the PNGs), except in a file whose `<image>` carries the test-only
//! attribute `tmi-test-defer="1"`: its raster layers are `deferred`, so the
//! host's `read-layer` path runs on the same files (the records must not
//! change).

pub mod model;
pub mod write;
pub mod zip;

#[cfg(any(target_arch = "wasm32", feature = "dev"))]
pub mod plugin;

#[cfg(target_arch = "wasm32")]
tmi_plugin_sdk::export_plugin!(plugin::Ora);

use model::*;

/// The plugin's identity (`reader.info`).
pub fn info() -> Info {
    Info {
        id: "ora",
        // TMI_ORA_VERSION at build time: a second version of the same
        // reader for the conformance scenario `plugins` (an upgrade, a
        // merge of records two versions made)
        version: match option_env!("TMI_ORA_VERSION") {
            Some(v) => v,
            None => "0.3.0",
        },
        format_name: "OpenRaster",
        extensions: &[".ora"],
        mime: Some("image/openraster"),
        probe_bytes: 64,
        // TMI_ORA_READ_ONLY=1 at build time: the same reader without its
        // writer (`can-write` false: the host treats it as a plugin that
        // cannot write layers back), as versions 0.1.0 and 0.2.0 of the
        // conformance scenario `plugins` are
        can_write: option_env!("TMI_ORA_READ_ONLY").is_none(),
    }
}

/// `reader.probe`: a zip whose first member is the stored `mimetype`
/// holding `image/openraster` (the OpenRaster rule).
pub fn probe(head: &[u8]) -> Confidence {
    let ok = head.len() >= 38
        && head.starts_with(b"PK\x03\x04")
        && &head[30..38] == b"mimetype"
        && head.windows(16).any(|w| w == b"image/openraster");
    if ok {
        Confidence::Yes
    } else {
        Confidence::No
    }
}

const MAX_STACK_XML: u32 = 16 << 20;

fn member(zip: &zip::Archive, name: &str) -> Result<Member, Error> {
    zip.member(name).ok_or_else(|| Error::Corrupt(format!("{name} is not in the archive")))
}

fn blend_of(op: &str) -> Option<Blend> {
    Some(match op {
        "svg:src-over" => Blend::Normal,
        "svg:multiply" => Blend::Multiply,
        "svg:screen" => Blend::Screen,
        "svg:overlay" => Blend::Overlay,
        "svg:darken" => Blend::Darken,
        "svg:lighten" => Blend::Lighten,
        "svg:color-dodge" => Blend::ColorDodge,
        "svg:color-burn" => Blend::ColorBurn,
        "svg:hard-light" => Blend::HardLight,
        "svg:soft-light" => Blend::SoftLight,
        "svg:difference" => Blend::Difference,
        "svg:plus" => Blend::LinearDodge,
        "svg:hue" => Blend::Hue,
        "svg:saturation" => Blend::Saturation,
        "svg:color" => Blend::Color,
        "svg:luminosity" => Blend::Luminosity,
        _ => return None,
    })
}

fn parse_int(v: Option<&str>, what: &str) -> Result<i64, Error> {
    match v {
        None => Ok(0),
        Some(s) => s.trim().parse::<i64>().map_err(|_| Error::Corrupt(format!("{what} {s:?} is not an integer"))),
    }
}

fn parse_opacity(v: Option<&str>) -> Result<f64, Error> {
    let s = v.unwrap_or("1");
    match s.trim().parse::<f64>() {
        Ok(x) if x.is_finite() && (0.0..=1.0).contains(&x) => Ok(x),
        _ => Err(Error::Corrupt(format!("opacity {s:?} is not a number from 0 to 1"))),
    }
}

/// The deferred layers' members, as `context` carries them: per token, the
/// span's offset and length (u64 LE) and 1 if it is deflated.
fn encode_context(members: &[Member]) -> Vec<u8> {
    let mut out = Vec::with_capacity(members.len() * 17);
    for m in members {
        out.extend_from_slice(&m.span.offset.to_le_bytes());
        out.extend_from_slice(&m.span.len.to_le_bytes());
        out.push(m.deflate as u8);
    }
    out
}

/// `reader.read-layer`: the member of deferred layer `token`.
pub fn read_layer(context: &[u8], token: u32) -> Result<Member, Error> {
    let at = token as usize * 17;
    let e = context.get(at..at + 17).ok_or_else(|| Error::Corrupt(format!("no deferred layer {token}")))?;
    let offset = u64::from_le_bytes(e[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(e[8..16].try_into().unwrap());
    Ok(Member { span: Span { offset, len }, deflate: e[16] == 1 })
}

/// `reader.read-structure`.
pub fn read_structure(host: &dyn Host) -> Result<Structure, Error> {
    read_all(host).map(|r| r.structure)
}

/// What the writer reads besides the structure.
pub(crate) struct Read {
    pub structure: Structure,
    /// Each raster layer's stable id and `src`.
    pub sources: Vec<(String, String)>,
    /// `<image tmi-test-write-fault=...>`: what the writer does wrong on purpose (write.rs).
    pub fault: Option<String>,
}

pub(crate) fn read_all(host: &dyn Host) -> Result<Read, Error> {
    let zip = zip::Archive::open(host)?;
    let stack = member(&zip, "stack.xml")?;
    let xml = if stack.deflate {
        host.inflate(&stack, MAX_STACK_XML).map_err(|e| Error::Corrupt(format!("stack.xml: {e}")))?
    } else {
        if stack.span.len > MAX_STACK_XML as u64 {
            return Err(Error::TooLarge("stack.xml over 16 MiB".into()));
        }
        host.read(stack.span.offset, stack.span.len as u32)
    };
    let text = std::str::from_utf8(&xml).map_err(|e| Error::Corrupt(format!("stack.xml is not UTF-8: {e}")))?;
    let doc = roxmltree::Document::parse(text).map_err(|e| Error::Corrupt(format!("stack.xml: {e}")))?;
    let image = doc.root_element();
    if image.tag_name().name() != "image" {
        return Err(Error::Corrupt(format!("stack.xml's root is <{}>, not <image>", image.tag_name().name())));
    }
    let dim = |key: &str| -> Result<u32, Error> {
        let v = image.attribute(key).ok_or_else(|| Error::Corrupt(format!("<image> has no {key}")))?;
        v.trim().parse::<u32>().map_err(|_| Error::Corrupt(format!("<image {key}={v:?}> is not a size")))
    };
    let (w, h) = (dim("w")?, dim("h")?);
    let dpi = match image.attribute("xres") {
        None => None,
        Some(v) => match v.trim().parse::<f64>() {
            Ok(d) if d.is_finite() => Some(d),
            _ => return Err(Error::Corrupt(format!("<image xres={v:?}> is not a number"))),
        },
    };
    let defer = image.attribute("tmi-test-defer") == Some("1");
    let root = image
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "stack")
        .ok_or_else(|| Error::Corrupt("<image> holds no <stack>".into()))?;

    let mut out = Walker { zip: &zip, layers: Vec::new(), warnings: Vec::new(), deferred: Vec::new(), defer, sources: Vec::new() };
    out.walk(root, None, "")?;
    let composite = match zip.member("mergedimage.png") {
        Some(m) => Composite::Image(m),
        None => Composite::Render,
    };
    let fault = image.attribute("tmi-test-write-fault").map(str::to_string);
    let structure = Structure {
        width: w,
        height: h,
        dpi,
        layers: out.layers,
        composite,
        context: encode_context(&out.deferred),
        warnings: out.warnings,
    };
    Ok(Read { structure, sources: out.sources, fault })
}

struct Walker<'a> {
    zip: &'a zip::Archive,
    layers: Vec<Layer>,
    warnings: Vec<(String, String)>,
    deferred: Vec<Member>,
    defer: bool,
    /// Each raster layer's stable id and src (the writer's map).
    sources: Vec<(String, String)>,
}

impl Walker<'_> {
    /// `elem`'s children, bottom to top (stack.xml lists them top first).
    fn walk(&mut self, elem: roxmltree::Node, parent: Option<u32>, path: &str) -> Result<(), Error> {
        let children: Vec<roxmltree::Node> = elem.children().filter(|n| n.is_element()).collect();
        for (i, child) in children.iter().rev().enumerate() {
            let tag = child.tag_name().name();
            if tag != "stack" && tag != "layer" {
                self.warnings.push(("element".into(), format!("<{tag}> in a stack is skipped")));
                continue;
            }
            let sid = format!("ora:{path}{i}");
            let op = child.attribute("composite-op").unwrap_or("svg:src-over");
            let blend = match blend_of(op) {
                Some(b) => b,
                None => {
                    self.warnings.push(("blend".into(), format!("unknown composite-op {op}; drawn as normal")));
                    Blend::Other
                }
            };
            let mut semantic: Vec<(String, String)> =
                child.attributes().map(|a| (a.name().to_string(), a.value().to_string())).collect();
            semantic.sort();
            let mut layer = Layer {
                id: sid.clone(),
                stable_id: sid,
                parent,
                name: child.attribute("name").unwrap_or("").to_string(),
                kind: Kind::Raster,
                visible: child.attribute("visibility").unwrap_or("visible") != "hidden",
                opacity: parse_opacity(child.attribute("opacity"))?,
                blend,
                offset_x: parse_int(child.attribute("x"), "x")?,
                offset_y: parse_int(child.attribute("y"), "y")?,
                pixels: Pixels::None,
                meta: vec![("composite_op".into(), op.to_string())],
                semantic,
                writable: false,
            };
            if tag == "stack" {
                let pass_through = child.attribute("isolation").unwrap_or("auto") == "auto" && op == "svg:src-over";
                layer.kind = Kind::Folder;
                if pass_through {
                    layer.blend = Blend::PassThrough;
                }
                self.layers.push(layer);
                let me = (self.layers.len() - 1) as u32;
                self.walk(*child, Some(me), &format!("{path}{i}/"))?;
            } else {
                let src = child.attribute("src").ok_or_else(|| Error::Corrupt("a <layer> has no src".into()))?;
                let m = member(self.zip, src)?;
                self.sources.push((layer.stable_id.clone(), src.to_string()));
                layer.writable = true;
                layer.pixels = if self.defer {
                    self.deferred.push(m);
                    Pixels::Deferred((self.deferred.len() - 1) as u32)
                } else {
                    Pixels::Ready(m)
                };
                self.layers.push(layer);
            }
        }
        Ok(())
    }
}
