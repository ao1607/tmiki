use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let raw = std::fs::read(&path).unwrap();
    let doc = tmi_clip::ClipDoc::open(&raw).unwrap();
    for _ in 0..3 {
        let t0 = Instant::now();
        let mut n = 0usize;
        let mut px = 0usize;
        for node in &doc.nodes {
            let Some(info) = doc.offscreen(node.render_mipmap) else { continue };
            let Some(exta) = doc.exta(info) else { continue };
            let a = tmi_clip::attr::parse_offscreen_attribute(&info.attribute).unwrap();
            let r = tmi_clip::tiles::decode_offscreen(&raw, exta.payload_off, exta.payload_size, a.tile_cols, a.tile_rows, a.pixel_width, a.pixel_height);
            if let Ok(d) = r {
                n += 1;
                px += d.w * d.h;
            }
        }
        println!("decoded {n} layers, {:.1} Mpx in {:.3}s", px as f64 / 1e6, t0.elapsed().as_secs_f64());
    }
}
