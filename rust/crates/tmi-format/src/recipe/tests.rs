//! The recipe decoder against a deliberately plain reference: every layout
//! (depths, channel groups, padding, partial 1-bit bytes, premultiplied,
//! tint), the three overlap modes, the transforms, images in tiles, and the
//! checks that fail an import.

use std::io::Write;

use super::*;
use crate::reader::ReadCx;
use tmi_pillow::image::Mode;

// ---------------------------------------------------------------------------
// helpers

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
}

fn env<'a>(source: &'a [u8], cx: &'a ReadCx) -> DecodeEnv<'a> {
    DecodeEnv { source, reader: None, cx }
}

fn inline(v: Vec<u8>) -> Bytes {
    Bytes::Inline(v)
}

fn raster_tile(x: i32, y: i32, w: u32, h: u32, data: Vec<u8>, layout: Layout) -> Tile {
    Tile { x, y, width: w, height: h, content: TileContent::Raster(Raster { data: inline(data), codecs: vec![], layout }) }
}

fn bitmap_of(tiles: Vec<Tile>, overlap: Overlap, w: u32, h: u32) -> Bitmap {
    Bitmap { size: Some((w, h)), source: PixelSource::Tiles(Tiled { tiles, overlap }) }
}

// ---------------------------------------------------------------------------
// the reference: one pixel at a time, from the definition

/// One sample of a row as a byte, bit by bit / byte by byte.
fn ref_sample(depth: Depth, row: &[u8], idx: usize) -> u8 {
    match depth {
        Depth::OneBitMsb => {
            let byte = row[idx / 8];
            let bit = 7 - (idx % 8);
            if (byte >> bit) & 1 == 1 {
                255
            } else {
                0
            }
        }
        Depth::Eight => row[idx],
        Depth::SixteenBe => (u16::from_be_bytes([row[2 * idx], row[2 * idx + 1]]) >> 8) as u8,
        Depth::SixteenLe => (u16::from_le_bytes([row[2 * idx], row[2 * idx + 1]]) >> 8) as u8,
        Depth::Float32Le => {
            let v = f32::from_le_bytes(row[4 * idx..4 * idx + 4].try_into().unwrap()) as f64;
            if v.is_nan() {
                0
            } else {
                (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8
            }
        }
    }
}

fn sample_bits(d: Depth) -> usize {
    match d {
        Depth::OneBitMsb => 1,
        Depth::Eight => 8,
        Depth::SixteenBe | Depth::SixteenLe => 16,
        Depth::Float32Le => 32,
    }
}

fn ref_unpremultiply(v: u8, a: u8) -> u8 {
    // Pillow's rgbA2rgba, written out
    if a == 0 || a == 255 {
        v
    } else {
        ((255 * v as i32) / a as i32).clamp(0, 255) as u8
    }
}

/// The RGBA of the `w` x `h` tile in `data`, pixel by pixel.
fn ref_decode(layout: &Layout, data: &[u8], w: usize, h: usize) -> Vec<u8> {
    // where each group starts
    let mut starts = vec![];
    let mut at = 0usize;
    let mut rowlens = vec![];
    for g in &layout.groups {
        let bits = w * g.channels.len() * sample_bits(g.depth);
        let rowlen = bits / 8 + usize::from(bits % 8 != 0);
        starts.push(at);
        rowlens.push(rowlen);
        at += h * (rowlen + layout.row_padding as usize);
    }
    assert_eq!(at, data.len(), "the reference's own length");
    let mut out = vec![];
    for y in 0..h {
        for x in 0..w {
            let mut px = [0u8, 0, 0, 255];
            let mut colour = false;
            for (gi, g) in layout.groups.iter().enumerate() {
                let rs = starts[gi] + y * (rowlens[gi] + layout.row_padding as usize);
                let row = &data[rs..rs + rowlens[gi]];
                for (k, ch) in g.channels.iter().enumerate() {
                    let v = ref_sample(g.depth, row, x * g.channels.len() + k);
                    match ch {
                        Channel::R => {
                            px[0] = v;
                            colour = true
                        }
                        Channel::G => {
                            px[1] = v;
                            colour = true
                        }
                        Channel::B => {
                            px[2] = v;
                            colour = true
                        }
                        Channel::Gray => {
                            px[0] = v;
                            px[1] = v;
                            px[2] = v;
                            colour = true
                        }
                        Channel::A => px[3] = v,
                        Channel::Pad => {}
                    }
                }
            }
            if layout.premultiplied {
                let a = px[3];
                for c in px.iter_mut().take(3) {
                    *c = ref_unpremultiply(*c, a);
                }
            }
            if let Some(t) = layout.tint {
                if !colour {
                    px[0] = t.r;
                    px[1] = t.g;
                    px[2] = t.b;
                }
                px[3] = ((px[3] as u32 * t.a as u32) / 255) as u8;
            }
            out.extend_from_slice(&px);
        }
    }
    out
}

fn layout_len(layout: &Layout, w: usize, h: usize) -> usize {
    layout
        .groups
        .iter()
        .map(|g| {
            let bits = w * g.channels.len() * sample_bits(g.depth);
            (bits / 8 + usize::from(bits % 8 != 0) + layout.row_padding as usize) * h
        })
        .sum()
}

const DEPTHS: [Depth; 5] = [Depth::OneBitMsb, Depth::Eight, Depth::SixteenBe, Depth::SixteenLe, Depth::Float32Le];

fn random_layout(rng: &mut Rng) -> Layout {
    use Channel::*;
    let sets: [&[Channel]; 9] = [&[A], &[Gray], &[R, G, B], &[B, G, R, Pad], &[R, G, B, A], &[Gray, A], &[Pad], &[B, G, R, A], &[A, Gray]];
    let ngroups = 1 + rng.below(3) as usize;
    let groups = (0..ngroups)
        .map(|_| Group { channels: sets[rng.below(sets.len() as u64) as usize].to_vec(), depth: DEPTHS[rng.below(5) as usize] })
        .collect();
    Layout {
        groups,
        row_padding: rng.below(4) as u32,
        tint: (rng.below(2) == 0).then(|| Rgba8 { r: rng.byte(), g: rng.byte(), b: rng.byte(), a: rng.byte() }),
        premultiplied: rng.below(3) == 0,
    }
}

/// Data for `layout`: random bytes, but float samples are drawn from a mix of
/// proper, out-of-range and NaN values so every branch of the conversion runs.
fn random_data(rng: &mut Rng, layout: &Layout, w: usize, h: usize) -> Vec<u8> {
    let mut data = rng.bytes(layout_len(layout, w, h));
    let mut at = 0usize;
    for g in &layout.groups {
        let bits = w * g.channels.len() * sample_bits(g.depth);
        let rowlen = bits / 8 + usize::from(bits % 8 != 0);
        if g.depth == Depth::Float32Le {
            for y in 0..h {
                for i in 0..w * g.channels.len() {
                    let v: f32 = match rng.below(6) {
                        0 => f32::NAN,
                        1 => -0.5,
                        2 => 1.75,
                        3 => 0.5,
                        _ => (rng.below(1001) as f32) / 1000.0,
                    };
                    let o = at + y * (rowlen + layout.row_padding as usize) + 4 * i;
                    data[o..o + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        at += h * (rowlen + layout.row_padding as usize);
    }
    data
}

// ---------------------------------------------------------------------------
// layouts

#[test]
fn every_layout_matches_the_reference() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let cx = ReadCx::new(None);
    let mut n = 0;
    for _ in 0..3000 {
        let layout = random_layout(&mut rng);
        let (w, h) = (1 + rng.below(19) as usize, 1 + rng.below(4) as usize);
        let data = random_data(&mut rng, &layout, w, h);
        let want = ref_decode(&layout, &data, w, h);
        let bm = bitmap_of(vec![raster_tile(0, 0, w as u32, h as u32, data, layout.clone())], Overlap::Replace, w as u32, h as u32);
        let got = decode_bitmap(&env(&[], &cx), &bm).unwrap();
        assert_eq!((got.width, got.height), (w, h));
        assert_eq!(got.data, want, "layout {layout:?} {w}x{h}");
        n += 1;
    }
    assert_eq!(n, 3000);
}

/// Each depth, alone, over every pixel count that leaves 1-bit rows with
/// unused bits at the end of the last byte.
#[test]
fn partial_bytes_of_one_bit_rows() {
    let cx = ReadCx::new(None);
    for w in 1..=17usize {
        for nch in 1..=3usize {
            let channels = [Channel::R, Channel::G, Channel::B][..nch].to_vec();
            let layout = Layout { groups: vec![Group { channels, depth: Depth::OneBitMsb }], row_padding: 1, tint: None, premultiplied: false };
            let rowlen = (w * nch).div_ceil(8);
            assert_eq!(layout::row_bytes(&layout.groups[0], w), rowlen);
            // the bits beyond the last sample are 1s: they must not leak into the next row
            let mut data = vec![];
            for y in 0..2usize {
                let mut row = vec![0u8; rowlen];
                for i in 0..rowlen * 8 {
                    let on = if i < w * nch { (i + y) % 3 == 0 } else { true };
                    if on {
                        row[i / 8] |= 0x80 >> (i % 8);
                    }
                }
                data.extend(row);
                data.push(0xEE);
            }
            let want = ref_decode(&layout, &data, w, 2);
            let bm = bitmap_of(vec![raster_tile(0, 0, w as u32, 2, data, layout)], Overlap::Replace, w as u32, 2);
            assert_eq!(decode_bitmap(&env(&[], &cx), &bm).unwrap().data, want, "w={w} nch={nch}");
        }
    }
}

#[test]
fn sixteen_bit_takes_the_high_byte_of_either_order() {
    let cx = ReadCx::new(None);
    let mk = |depth, data: Vec<u8>| {
        let layout = Layout { groups: vec![Group { channels: vec![Channel::Gray], depth }], row_padding: 0, tint: None, premultiplied: false };
        let bm = bitmap_of(vec![raster_tile(0, 0, 2, 1, data, layout)], Overlap::Replace, 2, 1);
        decode_bitmap(&env(&[], &cx), &bm).unwrap().data
    };
    assert_eq!(mk(Depth::SixteenBe, vec![0x12, 0x34, 0xFF, 0x00]), [0x12, 0x12, 0x12, 255, 0xFF, 0xFF, 0xFF, 255]);
    assert_eq!(mk(Depth::SixteenLe, vec![0x34, 0x12, 0x00, 0xFF]), [0x12, 0x12, 0x12, 255, 0xFF, 0xFF, 0xFF, 255]);
}

#[test]
fn premultiplied_is_pillows_rgba_a_to_rgba() {
    let cx = ReadCx::new(None);
    let layout = Layout {
        groups: vec![Group { channels: vec![Channel::R, Channel::G, Channel::B, Channel::A], depth: Depth::Eight }],
        row_padding: 0,
        tint: None,
        premultiplied: true,
    };
    // every alpha, with colour values at, below and above alpha
    let mut data = vec![];
    for a in 0..=255u16 {
        data.extend([a as u8 / 2, a as u8, 255, a as u8]);
    }
    let bm = bitmap_of(vec![raster_tile(0, 0, 256, 1, data.clone(), layout.clone())], Overlap::Replace, 256, 1);
    let got = decode_bitmap(&env(&[], &cx), &bm).unwrap().data;
    assert_eq!(got, ref_decode(&layout, &data, 256, 1));
    // spot values: alpha 0 and 255 keep the colour, 128 scales by 255/128 and clips
    assert_eq!(&got[0..4], &[0, 0, 255, 0]);
    assert_eq!(&got[255 * 4..], &[127, 255, 255, 255]);
    assert_eq!(&got[128 * 4..129 * 4], &[(255 * 64 / 128) as u8, 255, 255, 128]);
}

#[test]
fn the_mdp2_and_clip_shapes() {
    let cx = ReadCx::new(None);
    // CLIP: [a] then [b, g, r, pad], 2x1
    let clip = Layout {
        groups: vec![
            Group { channels: vec![Channel::A], depth: Depth::Eight },
            Group { channels: vec![Channel::B, Channel::G, Channel::R, Channel::Pad], depth: Depth::Eight },
        ],
        row_padding: 0,
        tint: None,
        premultiplied: false,
    };
    let bm = bitmap_of(vec![raster_tile(0, 0, 2, 1, vec![10, 20, 3, 2, 1, 0, 6, 5, 4, 0], clip)], Overlap::Replace, 2, 1);
    assert_eq!(decode_bitmap(&env(&[], &cx), &bm).unwrap().data, [1, 2, 3, 10, 4, 5, 6, 20]);
    // MDP2 1bpp: the bits are alpha, the colour is the layer's tint
    let one = Layout {
        groups: vec![Group { channels: vec![Channel::A], depth: Depth::OneBitMsb }],
        row_padding: 0,
        tint: Some(Rgba8 { r: 9, g: 8, b: 7, a: 200 }),
        premultiplied: false,
    };
    let bm = bitmap_of(vec![raster_tile(0, 0, 3, 1, vec![0b1010_0000], one)], Overlap::Replace, 3, 1);
    assert_eq!(decode_bitmap(&env(&[], &cx), &bm).unwrap().data, [9, 8, 7, 200, 9, 8, 7, 0, 9, 8, 7, 200]);
}

#[test]
fn a_wrong_data_length_fails() {
    let cx = ReadCx::new(None);
    let layout = Layout { groups: vec![Group { channels: vec![Channel::A], depth: Depth::Eight }], row_padding: 0, tint: None, premultiplied: false };
    for n in [3usize, 5] {
        let bm = bitmap_of(vec![raster_tile(0, 0, 2, 2, vec![0; n], layout.clone())], Overlap::Replace, 2, 2);
        assert!(matches!(decode_bitmap(&env(&[], &cx), &bm), Err(ReadError::Corrupt(_))), "{n} bytes");
    }
    let none = Layout { groups: vec![], ..layout };
    let bm = bitmap_of(vec![raster_tile(0, 0, 2, 2, vec![], none)], Overlap::Replace, 2, 2);
    assert!(decode_bitmap(&env(&[], &cx), &bm).is_err());
}

// ---------------------------------------------------------------------------
// tiles and overlap

fn ref_over(dst: [u8; 4], src: [u8; 4]) -> [u8; 4] {
    // Pillow's alpha_composite of one pixel, through tmi-pillow's kernel on a 1x1 image
    let mut d = dst.to_vec();
    tmi_pillow::rgba::over(&mut d, 1, 1, &src, 1, 1, 0, 0);
    [d[0], d[1], d[2], d[3]]
}

struct RefTile {
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    px: Option<Vec<u8>>,
}

/// The reference painter: pixel by pixel, in tile order.
fn ref_paint(tiles: &[RefTile], overlap: Overlap, bw: usize, bh: usize) -> Result<Vec<u8>, ()> {
    let mut canvas = vec![[0u8; 4]; bw * bh];
    let mut written = vec![false; bw * bh];
    for t in tiles {
        let Some(px) = &t.px else { continue };
        for ty in 0..t.h {
            for tx in 0..t.w {
                let (cx, cy) = (t.x as i64 + tx as i64, t.y as i64 + ty as i64);
                if cx < 0 || cy < 0 || cx >= bw as i64 || cy >= bh as i64 {
                    continue;
                }
                let i = cy as usize * bw + cx as usize;
                let s = &px[(ty * t.w + tx) * 4..(ty * t.w + tx) * 4 + 4];
                let s = [s[0], s[1], s[2], s[3]];
                match overlap {
                    Overlap::Replace => canvas[i] = s,
                    Overlap::Over => canvas[i] = ref_over(canvas[i], s),
                    Overlap::Reject => {
                        if written[i] {
                            return Err(());
                        }
                        canvas[i] = s;
                    }
                }
                written[i] = true;
            }
        }
    }
    Ok(canvas.concat())
}

fn rgba8_layout() -> Layout {
    Layout {
        groups: vec![Group { channels: vec![Channel::R, Channel::G, Channel::B, Channel::A], depth: Depth::Eight }],
        row_padding: 0,
        tint: None,
        premultiplied: false,
    }
}

fn random_tiles(rng: &mut Rng, bw: usize, bh: usize, disjoint: bool) -> (Vec<Tile>, Vec<RefTile>) {
    let n = 1 + rng.below(8) as usize;
    let (mut tiles, mut refs) = (vec![], vec![]);
    for i in 0..n {
        let (w, h) = (1 + rng.below(6) as usize, 1 + rng.below(6) as usize);
        let (x, y) = if disjoint {
            // a grid of 8x8 cells, a tile in the cell, some sticking out of the bitmap
            ((i as i32 % 4) * 8 - 2 + rng.below(2) as i32, (i as i32 / 4) * 8 - 1 + rng.below(2) as i32)
        } else {
            (rng.below(bw as u64 + 6) as i32 - 3, rng.below(bh as u64 + 6) as i32 - 3)
        };
        let (content, px) = match rng.below(4) {
            0 => (TileContent::Empty, None),
            1 => {
                let c = Rgba8 { r: rng.byte(), g: rng.byte(), b: rng.byte(), a: [0, 255, rng.byte()][rng.below(3) as usize] };
                (TileContent::Fill(c), Some([c.r, c.g, c.b, c.a].repeat(w * h)))
            }
            _ => {
                // semi-transparent pixels make `over` differ from `replace`
                let mut d = rng.bytes(w * h * 4);
                for p in d.chunks_exact_mut(4) {
                    if rng.below(3) == 0 {
                        p[3] = 255;
                    } else if rng.below(4) == 0 {
                        p[3] = 0;
                    }
                }
                (TileContent::Raster(Raster { data: inline(d.clone()), codecs: vec![], layout: rgba8_layout() }), Some(d))
            }
        };
        tiles.push(Tile { x, y, width: w as u32, height: h as u32, content });
        refs.push(RefTile { x, y, w, h, px });
    }
    (tiles, refs)
}

#[test]
fn replace_over_and_reject_match_the_reference() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let cx = ReadCx::new(None);
    let (mut rejects, mut accepts) = (0, 0);
    for round in 0..600 {
        let (bw, bh) = (1 + rng.below(30) as usize, 1 + rng.below(20) as usize);
        for overlap in [Overlap::Replace, Overlap::Over, Overlap::Reject] {
            let (tiles, refs) = random_tiles(&mut rng, bw, bh, round % 3 == 0);
            let want = ref_paint(&refs, overlap, bw, bh);
            let bm = bitmap_of(tiles, overlap, bw as u32, bh as u32);
            let got = decode_bitmap(&env(&[], &cx), &bm);
            match want {
                Ok(w) => {
                    assert_eq!(got.unwrap().data, w, "{overlap:?} round {round}");
                    accepts += 1;
                }
                Err(()) => {
                    assert!(matches!(got, Err(ReadError::Corrupt(_))), "{overlap:?} round {round}: reject must fail");
                    rejects += 1;
                }
            }
        }
    }
    assert!(rejects > 20 && accepts > 500, "the generator covers both outcomes ({accepts} / {rejects})");
}

#[test]
fn over_differs_from_replace_on_translucent_tiles() {
    let cx = ReadCx::new(None);
    let fill = |a| TileContent::Fill(Rgba8 { r: 200, g: 100, b: 50, a });
    let tiles = |o| {
        bitmap_of(
            vec![
                Tile { x: 0, y: 0, width: 2, height: 1, content: fill(255) },
                Tile { x: 1, y: 0, width: 2, height: 1, content: TileContent::Fill(Rgba8 { r: 0, g: 0, b: 255, a: 128 }) },
            ],
            o,
            3,
            1,
        )
    };
    let replace = decode_bitmap(&env(&[], &cx), &tiles(Overlap::Replace)).unwrap().data;
    let over = decode_bitmap(&env(&[], &cx), &tiles(Overlap::Over)).unwrap().data;
    assert_eq!(&replace[4..8], &[0, 0, 255, 128]);
    assert_eq!(&over[4..8], &ref_over([200, 100, 50, 255], [0, 0, 255, 128]));
    assert_ne!(replace, over);
    assert_eq!(&over[8..12], &[0, 0, 255, 128], "over a transparent cell is the tile itself");
    // reject: disjoint tiles pass, touching ones too, overlapping ones fail
    let reject = |x2| {
        bitmap_of(
            vec![
                Tile { x: 0, y: 0, width: 2, height: 1, content: fill(255) },
                Tile { x: x2, y: 0, width: 2, height: 1, content: fill(255) },
            ],
            Overlap::Reject,
            4,
            1,
        )
    };
    assert!(decode_bitmap(&env(&[], &cx), &reject(2)).is_ok());
    assert!(decode_bitmap(&env(&[], &cx), &reject(1)).is_err());
    // an overlap outside the bitmap is not an overlap
    let outside = bitmap_of(
        vec![
            Tile { x: 5, y: 0, width: 2, height: 1, content: fill(255) },
            Tile { x: 6, y: 0, width: 2, height: 1, content: fill(255) },
        ],
        Overlap::Reject,
        4,
        1,
    );
    assert!(decode_bitmap(&env(&[], &cx), &outside).is_ok());
}

#[test]
fn bitmap_cuts_what_sticks_out_and_empty_is_transparent() {
    let cx = ReadCx::new(None);
    let t = Tile { x: -1, y: -1, width: 2, height: 2, content: TileContent::Fill(Rgba8 { r: 1, g: 2, b: 3, a: 255 }) };
    let got = decode_bitmap(&env(&[], &cx), &bitmap_of(vec![t], Overlap::Replace, 2, 2)).unwrap();
    assert_eq!(got.data, [1, 2, 3, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let e = Bitmap { size: Some((2, 1)), source: PixelSource::Empty };
    assert_eq!(decode_bitmap(&env(&[], &cx), &e).unwrap().data, [0; 8]);
    // tiles without a size
    let t = Tile { x: 0, y: 0, width: 1, height: 1, content: TileContent::Empty };
    let no_size = Bitmap { size: None, source: PixelSource::Tiles(Tiled { tiles: vec![t], overlap: Overlap::Replace }) };
    assert!(decode_bitmap(&env(&[], &cx), &no_size).is_err());
}

#[test]
fn raw_pixels_are_placed_and_checked() {
    let cx = ReadCx::new(None);
    let raw = |x, y, w, h, rgba: Vec<u8>, size| Bitmap { size, source: PixelSource::Raw(RawPixels { x, y, width: w, height: h, rgba }) };
    let got = decode_bitmap(&env(&[], &cx), &raw(1, 0, 1, 1, vec![9, 9, 9, 9], Some((3, 1)))).unwrap();
    assert_eq!(got.data, [0, 0, 0, 0, 9, 9, 9, 9, 0, 0, 0, 0]);
    let got = decode_bitmap(&env(&[], &cx), &raw(0, 0, 1, 1, vec![9, 9, 9, 9], None)).unwrap();
    assert_eq!((got.width, got.height), (1, 1));
    assert!(decode_bitmap(&env(&[], &cx), &raw(0, 0, 2, 1, vec![9; 4], None)).is_err());
}

#[test]
fn the_pixel_limit_applies_to_bitmaps_and_tiles() {
    let cx = ReadCx::new(Some(100));
    let big = Bitmap { size: Some((11, 10)), source: PixelSource::Empty };
    assert!(matches!(decode_bitmap(&env(&[], &cx), &big), Err(ReadError::TooLarge(_))));
    let fits = Bitmap { size: Some((10, 10)), source: PixelSource::Empty };
    assert!(decode_bitmap(&env(&[], &cx), &fits).is_ok());
    let tile = Tile { x: 0, y: 0, width: 101, height: 1, content: TileContent::Fill(Rgba8 { r: 0, g: 0, b: 0, a: 0 }) };
    assert!(matches!(decode_bitmap(&env(&[], &cx), &bitmap_of(vec![tile], Overlap::Replace, 10, 10)), Err(ReadError::TooLarge(_))));
}

// ---------------------------------------------------------------------------
// transforms

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn all_a_layout() -> Layout {
    Layout { groups: vec![Group { channels: vec![Channel::A], depth: Depth::Eight }], row_padding: 0, tint: None, premultiplied: false }
}

fn one_tile(data: Bytes, codecs: Vec<Codec>, w: u32, h: u32) -> Bitmap {
    bitmap_of(
        vec![Tile { x: 0, y: 0, width: w, height: h, content: TileContent::Raster(Raster { data, codecs, layout: all_a_layout() }) }],
        Overlap::Replace,
        w,
        h,
    )
}

fn alphas(b: &DecodedBitmap) -> Vec<u8> {
    b.data.chunks_exact(4).map(|p| p[3]).collect()
}

#[test]
fn deflate_family_and_zstd() {
    let mut rng = Rng(77);
    let cx = ReadCx::new(None);
    let raw: Vec<u8> = (0..64).map(|i| if i % 5 == 0 { rng.byte() } else { 7 }).collect();
    let z = zlib(&raw);
    let deflate = {
        let mut e = flate2::write::DeflateEncoder::new(vec![], flate2::Compression::default());
        e.write_all(&raw).unwrap();
        e.finish().unwrap()
    };
    let gzip = {
        let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        e.write_all(&raw).unwrap();
        e.finish().unwrap()
    };
    let zstd_bytes = zstd::bulk::compress(&raw, 3).unwrap();
    for (codec, data) in [(Codec::Zlib, z.clone()), (Codec::Deflate, deflate), (Codec::Gzip, gzip), (Codec::Zstd, zstd_bytes)] {
        let got = decode_bitmap(&env(&[], &cx), &one_tile(inline(data), vec![codec], 8, 8)).unwrap();
        assert_eq!(alphas(&got), raw, "{codec:?}");
    }
    // a damaged stream, and one that expands to the wrong size
    let mut bad = z.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1; // adler32
    assert!(decode_bitmap(&env(&[], &cx), &one_tile(inline(bad), vec![Codec::Zlib], 8, 8)).is_err());
    assert!(decode_bitmap(&env(&[], &cx), &one_tile(inline(z.clone()), vec![Codec::Zlib], 8, 7)).is_err(), "too long");
    assert!(decode_bitmap(&env(&[], &cx), &one_tile(inline(z), vec![Codec::Zlib], 8, 9)).is_err(), "too short");
}

#[test]
fn spans_come_from_the_file_and_must_be_inside_it() {
    let cx = ReadCx::new(None);
    let raw: Vec<u8> = (0..16).collect();
    let z = zlib(&raw);
    let mut file = vec![0xAAu8; 5];
    let start = file.len() as u64;
    file.extend_from_slice(&z);
    file.extend_from_slice(&[0xBB; 7]);
    let span = Bytes::Span(Span { offset: start, len: z.len() as u64 });
    let got = decode_bitmap(&env(&file, &cx), &one_tile(span, vec![Codec::Zlib], 4, 4)).unwrap();
    assert_eq!(alphas(&got), raw);
    // the same bytes split over two spans
    let half = z.len() as u64 / 2;
    let split = Bytes::Spans(vec![Span { offset: start, len: half }, Span { offset: start + half, len: z.len() as u64 - half }]);
    let got = decode_bitmap(&env(&file, &cx), &one_tile(split, vec![Codec::Zlib], 4, 4)).unwrap();
    assert_eq!(alphas(&got), raw);
    // outside the file
    for bad in [Span { offset: file.len() as u64 - 2, len: 3 }, Span { offset: u64::MAX, len: 2 }] {
        assert!(decode_bitmap(&env(&file, &cx), &one_tile(Bytes::Span(bad), vec![], 1, 1)).is_err());
    }
}

#[test]
fn lz4_lzf_and_packbits() {
    let cx = ReadCx::new(None);
    let raw: Vec<u8> = (0..16).map(|i| [1, 1, 1, 1, 2, 3, 4, 5][i % 8]).collect();
    // LZ4: 8 literals, then a match of 4+4 bytes at offset 8 (the block ends with the match)
    let lz4 = {
        let mut v = vec![0x84u8];
        v.extend([1, 1, 1, 1, 2, 3, 4, 5]);
        v.extend([8, 0]);
        v
    };
    let got = decode_bitmap(&env(&[], &cx), &one_tile(inline(lz4), vec![Codec::Lz4Block(16)], 4, 4)).unwrap();
    assert_eq!(alphas(&got), raw);
    // a literal-only block with a 20-byte length (15 + 5 in the extension byte)
    let lit: Vec<u8> = (0..20).collect();
    let mut block = vec![0xF0u8, 5];
    block.extend(&lit);
    assert_eq!(transform::lz4_block(&block, 20).unwrap(), lit);
    // overlapping match: 1 literal, then offset 1 length 4+10 repeats it
    let rle = vec![0x1Au8, 9, 1, 0];
    assert_eq!(transform::lz4_block(&rle, 15).unwrap(), vec![9u8; 15]);
    // errors: wrong announced size, offset before the start
    assert!(transform::lz4_block(&block, 21).is_err());
    assert!(transform::lz4_block(&block, 19).is_err());
    assert!(transform::lz4_block(&[0x10, 1, 5, 0], 9).is_err());

    // LZF: literals (ctrl = n-1), then back references
    let mut lzf = vec![7u8]; // 8 literals
    lzf.extend([1, 1, 1, 1, 2, 3, 4, 5]);
    lzf.extend([0x80 | 0x00, 7]); // len field 4 -> 4+2 = 6 bytes, distance (0<<8)+7+1 = 8
    lzf.extend([0x20 | 0x00, 7]); // len field 1 -> 3 bytes, distance 8
    assert_eq!(transform::lzf(&lzf, 17).unwrap(), [raw.clone(), vec![1]].concat());
    // the long form: len field 7 + extension
    let mut long = vec![0u8, 9]; // 1 literal
    long.extend([0xE0, 3, 0]); // 7 + 3 + 2 = 12 bytes at distance 1
    assert_eq!(transform::lzf(&long, 13).unwrap(), vec![9u8; 13]);
    assert!(transform::lzf(&long, 14).is_err());
    assert!(transform::lzf(&[0x20, 5], 3).is_err(), "reference before the start");

    // PackBits: literal run, repeat run, a no-op byte
    let pb = vec![3u8, 10, 11, 12, 13, 0xFD, 7, 0x80, 0, 99]; // 4 literals, 7 x4, noop, 1 literal
    assert_eq!(transform::packbits(&pb, 9).unwrap(), [10, 11, 12, 13, 7, 7, 7, 7, 99]);
    assert!(transform::packbits(&pb, 8).is_err());
    assert!(transform::packbits(&pb, 10).is_err());
    assert!(transform::packbits(&[5, 1, 2], 6).is_err(), "literals past the input");
    let got = decode_bitmap(&env(&[], &cx), &one_tile(inline(pb), vec![Codec::Packbits(9)], 3, 3)).unwrap();
    assert_eq!(alphas(&got), [10, 11, 12, 13, 7, 7, 7, 7, 99]);
}

/// The forward PNG filters, written out for the test.
fn png_filter(rows: &[Vec<u8>], bpp: usize, types: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    for (y, row) in rows.iter().enumerate() {
        let ft = types[y % types.len()];
        out.push(ft);
        for i in 0..row.len() {
            let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
            let b = if y > 0 { rows[y - 1][i] as i32 } else { 0 };
            let c = if y > 0 && i >= bpp { rows[y - 1][i - bpp] as i32 } else { 0 };
            let pred = match ft {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => unreachable!(),
            };
            out.push((row[i] as i32 - pred) as u8);
        }
    }
    out
}

#[test]
fn predictors_undo_the_filters() {
    let mut rng = Rng(4242);
    for bpp in [1usize, 3, 4] {
        let rb = 12 * bpp;
        let rows: Vec<Vec<u8>> = (0..7).map(|_| rng.bytes(rb)).collect();
        for types in [&[0u8][..], &[1], &[2], &[3], &[4], &[0, 1, 2, 3, 4, 4, 3]] {
            let filtered = png_filter(&rows, bpp, types);
            let p = Predictor { kind: PredictorKind::PngRows, row_bytes: rb as u32, bytes_per_pixel: bpp as u8 };
            assert_eq!(transform::predictor(&filtered, &p).unwrap(), rows.concat(), "png bpp={bpp} {types:?}");
        }
        // TIFF: each row holds differences with the pixel before
        let mut diff = vec![];
        for row in &rows {
            for i in 0..rb {
                diff.push(if i >= bpp { row[i].wrapping_sub(row[i - bpp]) } else { row[i] });
            }
        }
        let p = Predictor { kind: PredictorKind::TiffHorizontal, row_bytes: rb as u32, bytes_per_pixel: bpp as u8 };
        assert_eq!(transform::predictor(&diff, &p).unwrap(), rows.concat(), "tiff bpp={bpp}");
    }
    let p = Predictor { kind: PredictorKind::PngRows, row_bytes: 4, bytes_per_pixel: 1 };
    assert!(transform::predictor(&[0; 9], &p).is_err(), "not whole rows");
    assert!(transform::predictor(&[7, 0, 0, 0, 0], &p).is_err(), "filter type 7");
}

#[test]
fn a_codec_chain_runs_in_order() {
    let cx = ReadCx::new(None);
    let mut rng = Rng(5);
    let (rb, h) = (6usize, 4usize);
    let rows: Vec<Vec<u8>> = (0..h).map(|_| rng.bytes(rb)).collect();
    let filtered = png_filter(&rows, 1, &[1, 2, 3, 4]);
    let codecs = vec![Codec::Zlib, Codec::Predictor(Predictor { kind: PredictorKind::PngRows, row_bytes: rb as u32, bytes_per_pixel: 1 })];
    let got = decode_bitmap(&env(&[], &cx), &one_tile(inline(zlib(&filtered)), codecs, rb as u32, h as u32)).unwrap();
    assert_eq!(alphas(&got), rows.concat());
}

struct Doubler;
impl FormatReader for Doubler {
    fn info(&self) -> &PluginInfo {
        unimplemented!()
    }
    fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
        Confidence::No
    }
    fn read_structure(&self, _: &crate::reader::Source, _: &ReadCx) -> Result<Structure, ReadError> {
        unimplemented!()
    }
    fn decode_chunk(&self, id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, ReadError> {
        assert_eq!(id, 3);
        assert_eq!(out_len as usize, input.len() * 2, "the host passes the size the tile needs");
        Ok(input.iter().flat_map(|&b| [b, b]).collect())
    }
}

#[test]
fn a_plugin_codec_is_the_readers_chunk_decoder() {
    let cx = ReadCx::new(None);
    let e = DecodeEnv { source: &[], reader: Some(&Doubler), cx: &cx };
    let got = decode_bitmap(&e, &one_tile(inline(vec![1, 2, 3, 4]), vec![Codec::Plugin(3)], 4, 2)).unwrap();
    assert_eq!(alphas(&got), [1, 1, 2, 2, 3, 3, 4, 4]);
    // no reader: unsupported, not a panic
    let err = decode_bitmap(&env(&[], &cx), &one_tile(inline(vec![1]), vec![Codec::Plugin(3)], 1, 1));
    assert!(matches!(err, Err(ReadError::Unsupported(_))));
    // the answer must not exceed the tile
    struct Greedy;
    impl FormatReader for Greedy {
        fn info(&self) -> &PluginInfo {
            unimplemented!()
        }
        fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
            Confidence::No
        }
        fn read_structure(&self, _: &crate::reader::Source, _: &ReadCx) -> Result<Structure, ReadError> {
            unimplemented!()
        }
        fn decode_chunk(&self, _: u32, _: &[u8], _: u32) -> Result<Vec<u8>, ReadError> {
            Ok(vec![0; 100])
        }
    }
    let e = DecodeEnv { source: &[], reader: Some(&Greedy), cx: &cx };
    assert!(decode_bitmap(&e, &one_tile(inline(vec![1]), vec![Codec::Plugin(1)], 2, 2)).is_err());
}

// ---------------------------------------------------------------------------
// images

fn png_of(w: usize, h: usize, rgba: Vec<u8>) -> Vec<u8> {
    tmi_image::png_enc::encode_png(&tmi_pillow::image::Image::new(Mode::RGBA, w, h, rgba), 0, &[], &[]).unwrap()
}

fn gradient(w: usize, h: usize) -> Vec<u8> {
    (0..w * h).flat_map(|i| [(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, 255 - (i % 4) as u8 * 60]).collect()
}

#[test]
fn an_image_is_decoded_by_the_host_and_placed() {
    let cx = ReadCx::new(None);
    let px = gradient(5, 3);
    let png = png_of(5, 3, px.clone());
    let mut file = vec![0u8; 9];
    file.extend_from_slice(&png);
    let img = EncodedImage { data: Bytes::Span(Span { offset: 9, len: png.len() as u64 }), pre: vec![], codec: ImageCodec::Png, frame: 0 };
    // no size: the image's own
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: img.clone() }) };
    let got = decode_bitmap(&env(&file, &cx), &b).unwrap();
    assert_eq!((got.width, got.height, &got.data), (5, 3, &px));
    // placed at (1, 1) on a bigger bitmap and cut on the right
    let b = Bitmap { size: Some((4, 4)), source: PixelSource::Image(PlacedImage { x: 1, y: 1, image: img.clone() }) };
    let got = decode_bitmap(&env(&file, &cx), &b).unwrap();
    let mut want = vec![0u8; 4 * 4 * 4];
    for y in 0..3 {
        for x in 0..3 {
            want[((y + 1) * 4 + x + 1) * 4..((y + 1) * 4 + x + 1) * 4 + 4].copy_from_slice(&px[(y * 5 + x) * 4..(y * 5 + x) * 4 + 4]);
        }
    }
    assert_eq!(got.data, want);
    // a zlib-wrapped image: `pre` strips it
    let wrapped = zlib(&png);
    let img2 = EncodedImage { data: inline(wrapped), pre: vec![Codec::Zlib], codec: ImageCodec::Png, frame: 0 };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: img2 }) };
    assert_eq!(decode_bitmap(&env(&file, &cx), &b).unwrap().data, px);
    // it must be what the codec says, and frame 0
    let wrong = EncodedImage { codec: ImageCodec::Gif, ..img.clone() };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: wrong }) };
    assert!(matches!(decode_bitmap(&env(&file, &cx), &b), Err(ReadError::Corrupt(_))));
    let frame1 = EncodedImage { frame: 1, ..img.clone() };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: frame1 }) };
    assert!(matches!(decode_bitmap(&env(&file, &cx), &b), Err(ReadError::Unsupported(_))));
    let qoi = EncodedImage { codec: ImageCodec::Qoi, ..img };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: qoi }) };
    assert!(matches!(decode_bitmap(&env(&file, &cx), &b), Err(ReadError::Unsupported(_))));
    // over the pixel limit: TooLarge, not a corrupt file
    let small = ReadCx::new(Some(10));
    let img = EncodedImage { data: Bytes::Span(Span { offset: 9, len: png.len() as u64 }), pre: vec![], codec: ImageCodec::Png, frame: 0 };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: img }) };
    assert!(matches!(decode_bitmap(&env(&file, &small), &b), Err(ReadError::TooLarge(_))));
}

#[test]
fn an_image_tile_must_be_the_size_of_the_tile() {
    let cx = ReadCx::new(None);
    let px = gradient(2, 2);
    let png = png_of(2, 2, px.clone());
    let content = |_: ()| TileContent::Image(EncodedImage { data: inline(png.clone()), pre: vec![], codec: ImageCodec::Png, frame: 0 });
    let ok = bitmap_of(vec![Tile { x: 1, y: 0, width: 2, height: 2, content: content(()) }], Overlap::Replace, 4, 2);
    let got = decode_bitmap(&env(&[], &cx), &ok).unwrap().data;
    assert_eq!(&got[4..12], &px[0..8]);
    let bad = bitmap_of(vec![Tile { x: 0, y: 0, width: 3, height: 2, content: content(()) }], Overlap::Replace, 4, 2);
    assert!(matches!(decode_bitmap(&env(&[], &cx), &bad), Err(ReadError::Corrupt(_))));
}

#[test]
fn an_offered_decode_is_taken_once_instead_of_decoding_again() {
    let cx = ReadCx::new(None);
    let px = gradient(3, 3);
    let png = png_of(3, 3, px);
    // the file is not a PNG at all: only the offered decode can answer
    let marker = tmi_pillow::image::Image::new(Mode::RGBA, 1, 1, vec![1, 2, 3, 4]);
    let key = crate::reader::ImageKey { offset: 0, len: png.len() as u64, codec: ImageCodec::Png, frame: 0 };
    cx.offer_decoded(key, marker);
    let img = EncodedImage { data: Bytes::Span(Span { offset: 0, len: png.len() as u64 }), pre: vec![], codec: ImageCodec::Png, frame: 0 };
    let b = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: img }) };
    let junk = vec![0u8; png.len()];
    assert_eq!(decode_bitmap(&env(&junk, &cx), &b).unwrap().data, [1, 2, 3, 4]);
    assert!(decode_bitmap(&env(&junk, &cx), &b).is_err(), "handed over once");
}

// ---------------------------------------------------------------------------
// the tight tile decode (`decode_layer_crop` / `sparse::decode`) against the full
// decode cut by `LayerCrop::from_full`

/// The full-path reference: today's whole-bitmap decode, then the crop.
fn full_crop(env: &DecodeEnv, b: &Bitmap) -> Result<tmi_import::crop::LayerCrop, String> {
    decode_bitmap(env, b).map(|d| tmi_import::crop::LayerCrop::from_full(&d.data, d.width, d.height)).map_err(|e| format!("{e:?}"))
}

fn same_crop(env: &DecodeEnv, b: &Bitmap, what: &str) -> bool {
    let want = full_crop(env, b);
    let got = decode_layer_crop(env, b).map_err(|e| format!("{e:?}"));
    match (&want, &got) {
        (Ok(w), Ok(g)) => {
            assert_eq!(g.size, w.size, "{what}: size");
            assert_eq!(g.rect, w.rect, "{what}: rect");
            assert_eq!(g.header(), w.header(), "{what}: header");
            assert!(g.rgba == w.rgba, "{what}: pixels");
            assert_eq!(g.digest(), w.digest(), "{what}: digest");
            assert_eq!(g.alpha_bbox(), w.alpha_bbox(), "{what}: alpha bbox");
            assert_eq!(g.encode(), w.encode(), "{what}: blob");
            true
        }
        (Err(w), Err(g)) => {
            assert_eq!(g, w, "{what}: error");
            false
        }
        _ => panic!("{what}: full {:?} / tight {:?}", want.as_ref().map(|c| c.header()), got.as_ref().map(|c| c.header())),
    }
}

/// A random tile: every content kind (empty, fills with alpha 0 and colour,
/// rasters of random layouts incl. one-bit, tint, groups and premultiplied,
/// some with bad data), at `x`, `y`.
fn random_sparse_tile(rng: &mut Rng, x: i32, y: i32, w: usize, h: usize, bad: bool) -> Tile {
    let content = match rng.below(6) {
        0 => TileContent::Empty,
        1 => TileContent::Fill(Rgba8 { r: rng.byte(), g: rng.byte(), b: rng.byte(), a: [0, 255, rng.byte()][rng.below(3) as usize] }),
        2 => {
            // mostly transparent RGBA, the rest opaque or translucent
            let mut d = vec![0u8; w * h * 4];
            for p in d.chunks_exact_mut(4) {
                match rng.below(5) {
                    0 => p.copy_from_slice(&[rng.byte(), rng.byte(), rng.byte(), 255]),
                    1 => p.copy_from_slice(&[rng.byte(), rng.byte(), rng.byte(), rng.byte()]),
                    2 => p.copy_from_slice(&[rng.byte(), 0, rng.byte(), 0]),
                    _ => {}
                }
            }
            TileContent::Raster(Raster { data: inline(d), codecs: vec![], layout: rgba8_layout() })
        }
        3 => {
            // MDP2's 1-bit / 8-bit alpha with a tint: colour where alpha is 0
            let depth = [Depth::OneBitMsb, Depth::Eight][rng.below(2) as usize];
            let layout = Layout {
                groups: vec![Group { channels: vec![Channel::A], depth }],
                row_padding: 0,
                tint: Some(Rgba8 { r: rng.byte(), g: rng.byte(), b: rng.byte(), a: [255, rng.byte()][rng.below(2) as usize] }),
                premultiplied: false,
            };
            let mut d = random_data(rng, &layout, w, h);
            if rng.below(2) == 0 {
                for v in &mut d {
                    *v &= if rng.below(3) == 0 { 0xFF } else { 0 };
                }
            }
            TileContent::Raster(Raster { data: inline(d), codecs: vec![], layout })
        }
        _ => {
            let layout = random_layout(rng);
            let mut d = random_data(rng, &layout, w, h);
            if bad {
                d.push(0);
            }
            let codecs = if rng.below(2) == 0 { vec![] } else { d = zlib(&d); vec![Codec::Zlib] };
            TileContent::Raster(Raster { data: inline(d), codecs, layout })
        }
    };
    Tile { x, y, width: w as u32, height: h as u32, content }
}

#[test]
fn the_tight_tile_decode_equals_the_full_one() {
    let mut rng = Rng(0x5EED_0F_5A_A55E);
    let cx = ReadCx::new(None);
    let (mut oks, mut errs) = (0, 0);
    for round in 0..1500 {
        let (bw, bh) = (1 + rng.below(40) as usize, 1 + rng.below(30) as usize);
        let grid = round % 2 == 0;
        let cell = 2 + rng.below(7) as usize;
        let n = rng.below(14) as usize;
        let mut tiles = Vec::new();
        for i in 0..n {
            let bad = rng.below(25) == 0;
            let (x, y, w, h) = if grid {
                // a cell grid with holes, repeats, oversized and outside tiles
                let cols = bw.div_ceil(cell) + 1;
                let c = if rng.below(6) == 0 && i > 0 { rng.below(i as u64) as usize } else { i };
                let (mut w, mut h) = (cell, cell);
                if rng.below(8) == 0 {
                    w += 1 + rng.below(cell as u64) as usize;
                }
                if rng.below(8) == 0 {
                    h += 1 + rng.below(cell as u64) as usize;
                }
                let (mut x, mut y) = (((c % cols) * cell) as i32, ((c / cols) * cell) as i32);
                if rng.below(12) == 0 {
                    x = -(w as i32) - rng.below(3) as i32;
                }
                if rng.below(12) == 0 {
                    y = (bh + rng.below(4) as usize) as i32;
                }
                (x, y, w, h)
            } else {
                let (w, h) = (rng.below(9) as usize, rng.below(9) as usize);
                (rng.below(bw as u64 + 12) as i32 - 6, rng.below(bh as u64 + 12) as i32 - 6, w, h)
            };
            tiles.push(random_sparse_tile(&mut rng, x, y, w, h, bad));
        }
        for overlap in [Overlap::Replace, Overlap::Over, Overlap::Reject] {
            let bm = bitmap_of(tiles.clone(), overlap, bw as u32, bh as u32);
            if same_crop(&env(&[], &cx), &bm, &format!("{overlap:?} round {round}")) {
                oks += 1;
            } else {
                errs += 1;
            }
        }
    }
    assert!(oks > 2500 && errs > 300, "the generator covers both outcomes ({oks} / {errs})");
}

#[test]
fn the_tight_tile_decode_keeps_the_error_order() {
    let cx = ReadCx::new(None);
    let fill = Tile { x: 0, y: 0, width: 2, height: 2, content: TileContent::Fill(Rgba8 { r: 1, g: 2, b: 3, a: 255 }) };
    let bad = |x| Tile { x, y: 0, width: 2, height: 2, content: TileContent::Raster(Raster { data: inline(vec![1]), codecs: vec![], layout: all_a_layout() }) };
    let empty = Tile { x: 0, y: 0, width: 2, height: 2, content: TileContent::Empty };
    let cases: Vec<(Vec<Tile>, Overlap, &str)> = vec![
        // a bad tile outside the bitmap still fails
        (vec![fill.clone(), bad(50)], Overlap::Over, "Corrupt(\"tile data is 1 bytes"),
        // the overlap before the bad tile is the error, and the other way round
        (vec![fill.clone(), Tile { x: 1, ..fill.clone() }, bad(9)], Overlap::Reject, "Corrupt(\"tiles overlap at (1, 0)"),
        (vec![fill.clone(), bad(9), Tile { x: 1, ..fill.clone() }], Overlap::Reject, "Corrupt(\"tile data is 1 bytes"),
        // a tile's own error comes before its overlap
        (vec![fill.clone(), bad(1)], Overlap::Reject, "Corrupt(\"tile data is 1 bytes"),
        // empty and zero-sized tiles are never decoded nor overlap
        (vec![fill.clone(), empty, Tile { width: 0, ..bad(0) }], Overlap::Reject, ""),
    ];
    for (i, (tiles, overlap, want)) in cases.into_iter().enumerate() {
        let bm = bitmap_of(tiles, overlap, 4, 2);
        let got = decode_layer_crop(&env(&[], &cx), &bm).map(|c| c.header()).map_err(|e| format!("{e:?}"));
        assert_eq!(full_crop(&env(&[], &cx), &bm).map(|c| c.header()), got.clone(), "case {i}");
        match got {
            Ok(_) => assert!(want.is_empty(), "case {i}: {got:?}"),
            Err(e) => assert!(!want.is_empty() && e.starts_with(want), "case {i}: {e}"),
        }
    }
}

#[test]
fn the_tight_tile_decode_allocates_what_is_painted() {
    // a 100000 x 100000 bitmap with one painted pixel: the full decode would
    // need 40 GB; the tight one holds the pixel
    let cx = ReadCx::new(None);
    let t = Tile { x: 70_000, y: 99_999, width: 3, height: 1, content: TileContent::Fill(Rgba8 { r: 0, g: 0, b: 0, a: 0 }) };
    let p = Tile { x: 70_001, y: 99_999, width: 1, height: 1, content: TileContent::Fill(Rgba8 { r: 9, g: 8, b: 7, a: 6 }) };
    let bm = Bitmap { size: Some((100_000, 100_000)), source: PixelSource::Tiles(Tiled { tiles: vec![t, p], overlap: Overlap::Over }) };
    let env = DecodeEnv { source: &[], reader: None, cx: &cx };
    // over the 1 GiB-pixel guard of the full decode: the same error both ways
    assert!(decode_layer_crop(&env, &bm).is_err());
    let bm = Bitmap { size: Some((30_000, 30_000)), source: PixelSource::Tiles(Tiled { tiles: vec![Tile { x: 20_000, y: 29_999, ..match &bm.source { PixelSource::Tiles(t) => t.tiles[1].clone(), _ => unreachable!() } }], overlap: Overlap::Over }) };
    let c = decode_layer_crop(&env, &bm).unwrap();
    assert_eq!(c.header(), "v1 30000 30000 20000 29999 20001 30000");
    assert_eq!(c.rgba, [9, 8, 7, 6]);
}

#[test]
fn a_truncated_zlib_stream_fails_even_when_its_bytes_are_all_there() {
    // zlib.decompress (Python) and the CLIP / MDP2 inflaters want the stream's end
    let cx = ReadCx::new(None);
    let data: Vec<u8> = (0..64u8).collect();
    let z = zlib(&data);
    for cut in [4usize, 1] {
        let short = z[..z.len() - cut].to_vec();
        let bm = one_tile(inline(short), vec![Codec::Zlib], 8, 8);
        assert!(decode_bitmap(&env(&[], &cx), &bm).is_err(), "cut {cut}");
    }
    let mut trailing = z.clone();
    trailing.extend_from_slice(b"junk");
    assert!(decode_bitmap(&env(&[], &cx), &one_tile(inline(trailing), vec![Codec::Zlib], 8, 8)).is_ok(), "bytes after the end are ignored");
}

/// What `sparse::decode` relies on to copy the first tile of a group: over a
/// transparent pixel, `over_pixel` gives the source when its alpha is not 0
/// (each channel depends only on itself and the alpha: every pair checked).
#[test]
fn over_pixel_onto_zero_is_the_source() {
    for a in 0..=255u8 {
        for c in 0..=255u8 {
            let mut d = [0u8; 4];
            tmi_pillow::over::over_pixel(&mut d, &[c, c, c, a]);
            assert_eq!(d, if a == 0 { [0; 4] } else { [c, c, c, a] }, "c={c} a={a}");
        }
    }
}

#[test]
fn a_cut_in_place_equals_a_crop() {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for _ in 0..3000 {
        let (w, h) = (1 + rng.below(70) as usize, 1 + rng.below(40) as usize);
        let buf = rng.bytes(w * h * 4);
        let (x0, y0) = (rng.below(w as u64) as usize, rng.below(h as u64) as usize);
        let (x1, y1) = (x0 + 1 + rng.below((w - x0) as u64) as usize, y0 + 1 + rng.below((h - y0) as u64) as usize);
        let want = tmi_pillow::rgba::crop(&buf, w, h, x0 as i64, y0 as i64, x1 as i64, y1 as i64);
        let mut got = buf.clone();
        sparse::cut_in_place(&mut got, w, (x0, y0, x1, y1));
        assert_eq!(got, want, "{w}x{h} cut to {:?}", (x0, y0, x1, y1));
    }
}

/// Many small tiles, few of them painted: the edge search of the tight
/// decode stops early on each side (chunks of a few tiles at a time).
#[test]
fn the_tight_tile_decode_equals_the_full_one_on_many_tiles() {
    let mut rng = Rng(0xFACE_B00C_0000_0001);
    let cx = ReadCx::new(None);
    for round in 0..300 {
        let (bw, bh) = (10 + rng.below(60) as usize, 10 + rng.below(50) as usize);
        let cell = 2 + rng.below(4) as usize;
        let (cols, rows) = (bw.div_ceil(cell) + 1, bh.div_ceil(cell) + 1);
        let mut tiles = Vec::new();
        for c in 0..cols * rows {
            if rng.below(4) == 0 {
                continue; // a hole
            }
            let (x, y) = (((c % cols) * cell) as i32 - 1, ((c / cols) * cell) as i32 - 1);
            let mut d = vec![0u8; cell * cell * 4];
            if rng.below(12) == 0 {
                for p in d.chunks_exact_mut(4) {
                    if rng.below(3) == 0 {
                        p.copy_from_slice(&[rng.byte(), rng.byte(), rng.byte(), [255, rng.byte(), 0][rng.below(3) as usize]]);
                    }
                }
            } else if rng.below(3) == 0 {
                d[0] = 7; // colour under a zero alpha
            }
            let content = TileContent::Raster(Raster { data: inline(d), codecs: vec![], layout: rgba8_layout() });
            tiles.push(Tile { x, y, width: cell as u32, height: cell as u32, content });
        }
        if round % 5 == 0 && !tiles.is_empty() {
            // a repeated tile: a group of two
            let k = rng.below(tiles.len() as u64) as usize;
            tiles.push(tiles[k].clone());
        }
        for overlap in [Overlap::Replace, Overlap::Over, Overlap::Reject] {
            let bm = bitmap_of(tiles.clone(), overlap, bw as u32, bh as u32);
            same_crop(&env(&[], &cx), &bm, &format!("{overlap:?} round {round}"));
        }
    }
}
