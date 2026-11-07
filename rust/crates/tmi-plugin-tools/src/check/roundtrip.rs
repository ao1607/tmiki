//! PLUGINS.md 7.1 item 4: the writer's round trip. For each layer it may
//! write: its own pixels written back (the records hold, the bytes should
//! too, and writing them again changes nothing), then test patterns (one
//! pixel at the far corner, all transparent, colour under alpha 0), each
//! through the host's check after writing (PLUGINS.md 5): the edited layer
//! holds the new pixels, everything else is as it was.

use tmi_format::pipeline::Importer;
use tmi_format::reader::{FormatWriter, ReadCx};
use tmi_format::recipe::{decode_bitmap, DecodeEnv};
use tmi_format::types::LayerPixels;
use tmi_format::write::{write_checked, Edit};
use tmi_import::{Rgba, Warnings};

use super::readers::{self, LIMITS};
use super::report::{Outcome, Report};

fn fail(r: &mut Report, check: &str, file: &str, at: &str, why: impl Into<String>) {
    r.add_in(check, file, at, Outcome::Fail(why.into()));
}

fn skip(r: &mut Report, check: &str, file: &str, at: &str, why: impl Into<String>) {
    r.add_in(check, file, at, Outcome::Skip(why.into()));
}

/// Layers written per file (the rest are said to be skipped).
pub const MAX_LAYERS: usize = 16;

fn write(writer: &dyn FormatWriter, importer: &Importer, name: &str, data: &[u8], sid: &str, pixels: Rgba) -> Result<Vec<u8>, String> {
    let edit = Edit { stable_id: sid.to_string(), pixels };
    write_checked(writer, importer, name, data, vec![edit], LIMITS, &mut Warnings::default()).map_err(|e| e.to_string())
}

fn patterns(w: usize, h: usize) -> Vec<(&'static str, Vec<u8>)> {
    let n = w * h * 4;
    let mut corner = vec![0u8; n];
    corner[n - 4..].copy_from_slice(&[255, 0, 0, 255]);
    let clear = vec![0u8; n];
    let mut hidden_colour = [40u8, 200, 90, 0].repeat(w * h);
    // one opaque pixel, so the layer is not all transparent
    hidden_colour[..4].copy_from_slice(&[0, 0, 0, 255]);
    vec![("one pixel at the far corner", corner), ("all transparent", clear), ("colour under alpha 0", hidden_colour)]
}

pub fn run(report: &mut Report, writer: &dyn FormatWriter, importer: &Importer, name: &str, data: &[u8], record: &readers::Imported) {
    const SAME: &str = "4 round trip: the same pixels";
    const AGAIN: &str = "4 round trip: written twice, the same bytes";
    const PATTERN: &str = "4 round trip: test patterns";
    let layers = readers::writable(&record.file);
    if layers.is_empty() {
        report.skip(SAME, Some(name), "no layer can be written (write: pixels and a bitmap)");
        return;
    }
    let structure = match readers::resolved_structure(writer, name, data) {
        Ok(s) => s,
        Err(e) => {
            report.fail(SAME, Some(name), format!("reading the structure: {e}"));
            return;
        }
    };
    let cx = ReadCx::new(None);
    let env = DecodeEnv { source: data, reader: Some(writer), cx: &cx };
    if layers.len() > MAX_LAYERS {
        report.skip(SAME, Some(name), format!("{} writable layers: the first {MAX_LAYERS} are written", layers.len()));
    }
    // layers whose own pixels, written back, gave other bytes (said once)
    let mut other_bytes: Vec<String> = Vec::new();
    for (sid, lname) in layers.iter().take(MAX_LAYERS) {
        let at = format!("layer {lname:?} ({sid})");
        let Some(layer) = structure.layers.iter().find(|l| &l.stable_id == sid) else {
            fail(report, SAME, name, &at, "the layer is in the record but not in the structure");
            continue;
        };
        let LayerPixels::Ready(bitmap) = &layer.pixels else { continue };
        let own = match decode_bitmap(&env, bitmap) {
            Ok(d) => Rgba { width: d.width, height: d.height, data: d.data },
            Err(e) => {
                fail(report, SAME, name, &at, format!("decoding its pixels: {e}"));
                continue;
            }
        };
        let (w, h) = (own.width, own.height);
        if w == 0 || h == 0 {
            skip(report, SAME, name, &at, "an empty bitmap");
            continue;
        }
        match write(writer, importer, name, data, sid, Rgba { width: w, height: h, data: own.data.clone() }) {
            Err(e) => {
                fail(report, SAME, name, &at, e);
            }
            Ok(once) => {
                report.add_in(SAME, name, &at, Outcome::Ok);
                if once != data {
                    other_bytes.push(lname.clone());
                }
                match write(writer, importer, name, &once, sid, Rgba { width: w, height: h, data: own.data.clone() }) {
                    Ok(twice) if twice == once => report.add_in(AGAIN, name, &at, Outcome::Ok),
                    Ok(twice) => fail(report, AGAIN, name, &at, format!("writing what it wrote again gave other bytes ({} vs {} bytes); a writer must be idempotent", twice.len(), once.len())),
                    Err(e) => fail(report, AGAIN, name, &at, format!("writing its own output again: {e}")),
                }
            }
        }
        for (what, px) in patterns(w, h) {
            let r = write(writer, importer, name, data, sid, Rgba { width: w, height: h, data: px }).map(|_| ()).map_err(|e| format!("{what}: {e}"));
            report.check_in(PATTERN, name, &at, r);
        }
    }
    if !other_bytes.is_empty() {
        report.warn(
            SAME,
            Some(name),
            format!("the records hold, but the bytes differ from the file's for {} layer(s) written with their own pixels (first {:?})", other_bytes.len(), other_bytes[0]),
        );
    }
}
