//! Time the Rust CLIP renderer on a file: `cargo run --release --example
//! render_clip -- <file.clip> [runs]`.

use std::time::Instant;

use tmi_render::clip::band::{render, Target};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: render_clip <file.clip> [runs]");
    let runs: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(5);
    let raw = std::fs::read(&path).unwrap();
    for _ in 0..runs {
        let t0 = Instant::now();
        let doc = tmi_clip::ClipDoc::open(&raw).unwrap();
        let t1 = Instant::now();
        let mut out = vec![0u8; doc.width * doc.height * 4];
        let r = render(&doc, &raw, [255, 255, 255, 255], &Target::Canvas, false, &mut out);
        let t2 = Instant::now();
        println!(
            "open {:.3}s render {:.3}s {}",
            (t1 - t0).as_secs_f64(),
            (t2 - t1).as_secs_f64(),
            if r.is_ok() { "ok" } else { "declined" }
        );
    }
}
