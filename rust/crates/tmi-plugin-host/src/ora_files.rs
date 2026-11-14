//! Hand-made OpenRaster files for the tests and the measurements (no file
//! is downloaded): a small zip writer, PNGs from tmiki's own encoder, and a
//! few documents that use what the ORA reader reads.

use tmi_pillow::image::{Image, Mode};

/// A zip member.
pub struct Member {
    pub name: String,
    pub data: Vec<u8>,
    pub deflate: bool,
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(b);
    c.sum()
}

fn deflate(b: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

/// A zip of `members`, in order (no zip64, DOS time 1980-01-01 00:00).
pub fn zip(members: &[Member]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for m in members {
        let stored = if m.deflate { deflate(&m.data) } else { m.data.clone() };
        let crc = crc32(&m.data);
        let method: u16 = if m.deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        let name = m.name.as_bytes();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x21u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        out.extend_from_slice(&(m.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&stored);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x21u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        central.extend_from_slice(&(m.data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A deterministic RGBA picture with every kind of alpha.
pub fn pattern(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let a = match (x + y + seed as usize) % 5 {
                0 => 0,
                1 => 255,
                _ => (s >> 24) as u8,
            };
            out.extend_from_slice(&[(x * 7 + seed as usize) as u8, (y * 11) as u8, (s >> 8) as u8, a]);
        }
    }
    out
}

pub fn png(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    tmi_image::png_enc::encode_png(&Image::new(Mode::RGBA, w, h, rgba.to_vec()), 0, &[], &[]).expect("encode a png")
}

/// What [`sample`] varies.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Members (stack.xml and the PNGs) deflated, else stored.
    pub deflate: bool,
    /// `mergedimage.png`: None, at the canvas size, or at another size.
    pub merged: Merged,
    /// `tmi-test-defer="1"`: the raster layers deferred.
    pub defer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Merged {
    None,
    Canvas,
    WrongSize,
}

const W: usize = 64;
const H: usize = 48;

/// A small document: a nested stack (isolated with a mode, and pass-through),
/// offsets, opacity, a hidden layer, an unknown composite-op, a non-ASCII
/// name and xres.
pub fn sample(o: Sample) -> Vec<u8> {
    sample_with(o, "")
}

/// [`sample`] with `attrs` added to `<image>` (e.g. ` tmi-test-write-fault="other-layer"`).
pub fn sample_with(o: Sample, attrs: &str) -> Vec<u8> {
    let defer = if o.defer { r#" tmi-test-defer="1""# } else { "" };
    let stack = format!(
        r#"<?xml version='1.0' encoding='UTF-8'?>
<image version="0.0.3" w="{W}" h="{H}" xres="144"{defer}{attrs}>
 <stack>
  <layer name="top" src="data/top.png" x="10" y="-4" opacity="0.5" composite-op="svg:multiply"/>
  <stack name="group" opacity="0.8" isolation="isolate" composite-op="svg:screen">
   <layer name="in group" src="data/g1.png" x="3" y="5" visibility="hidden"/>
   <layer name="in group 2" src="data/g2.png" composite-op="svg:overlay"/>
  </stack>
  <stack name="through">
   <layer name="odd" src="data/odd.png" x="-7" y="30" composite-op="svg:xor"/>
  </stack>
  <layer name="背景" src="data/bg.png"/>
 </stack>
</image>
"#
    );
    let layers = [("top", 20, 12, 1), ("g1", 30, 20, 2), ("g2", 64, 48, 3), ("odd", 25, 25, 4), ("bg", 64, 48, 5)];
    let mut members = vec![
        Member { name: "mimetype".into(), data: b"image/openraster".to_vec(), deflate: false },
        Member { name: "stack.xml".into(), data: stack.into_bytes(), deflate: o.deflate },
    ];
    for (n, w, h, seed) in layers {
        members.push(Member { name: format!("data/{n}.png"), data: png(w, h, &pattern(w, h, seed)), deflate: o.deflate });
    }
    match o.merged {
        Merged::None => {}
        Merged::Canvas => members.push(Member { name: "mergedimage.png".into(), data: png(W, H, &pattern(W, H, 9)), deflate: o.deflate }),
        Merged::WrongSize => members.push(Member { name: "mergedimage.png".into(), data: png(W / 2, H, &pattern(W / 2, H, 9)), deflate: o.deflate }),
    }
    members.push(Member { name: "Thumbnails/thumbnail.png".into(), data: png(16, 12, &pattern(16, 12, 8)), deflate: false });
    zip(&members)
}

/// `n` layers of `side`×`side` in one flat stack (`n` up to 10,000 for
/// PLUGINS.md 6.4 item 3), stored or deflated, deferred or not, rendered
/// with generic-v1 (no mergedimage).
pub fn many_layers(n: usize, side: usize, deflate: bool, defer: bool) -> Vec<u8> {
    let mut xml = format!(
        "<?xml version='1.0' encoding='UTF-8'?>\n<image w=\"256\" h=\"256\"{}>\n <stack>\n",
        if defer { r#" tmi-test-defer="1""# } else { "" }
    );
    for i in 0..n {
        xml.push_str(&format!("  <layer name=\"l{i}\" src=\"data/{i}.png\" x=\"{}\" y=\"{}\"/>\n", (i * 37) % 200, (i * 53) % 200));
    }
    xml.push_str(" </stack>\n</image>\n");
    let mut members = vec![
        Member { name: "mimetype".into(), data: b"image/openraster".to_vec(), deflate: false },
        Member { name: "stack.xml".into(), data: xml.into_bytes(), deflate },
    ];
    // one picture per 16 layers keeps 10,000 layers quick to make
    let pics: Vec<Vec<u8>> = (0..16).map(|s| png(side, side, &pattern(side, side, s))).collect();
    for i in 0..n {
        members.push(Member { name: format!("data/{i}.png"), data: pics[i % 16].clone(), deflate });
    }
    zip(&members)
}
