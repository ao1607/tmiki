//! Hostile plugin output (PLUGINS.md 5): sizes a plugin announces or a plan
//! it returns are refused before the host allocates for them.

use crate::reader::ReadCx;
use crate::recipe::{decode_bitmap, transform, DecodeEnv};
use crate::types::*;
use crate::validate;
use crate::write::{stitch, Pixels, WriteCx, MAX_PLAN_PIECES};
use std::borrow::Cow;

fn plan(pieces: Vec<Piece>) -> WritePlan {
    WritePlan { pieces, warnings: vec![] }
}

fn cx2x2() -> WriteCx {
    WriteCx::new(vec![Pixels { width: 2, height: 2, rgba: vec![255; 16] }], None)
}

fn rgba_layout(row_padding: u32) -> Layout {
    Layout { groups: vec![Group { channels: vec![Channel::R, Channel::G, Channel::B, Channel::A], depth: Depth::Eight }], row_padding, tint: None, premultiplied: false }
}

fn env(cx: &ReadCx) -> DecodeEnv<'_> {
    DecodeEnv { source: &[], reader: None, cx }
}

// 1. a plan whose source pieces repeat a span

#[test]
fn a_plan_that_repeats_source_spans_is_refused_before_it_is_joined() {
    let src = vec![7u8; 1 << 20];
    let whole = Span { offset: 0, len: src.len() as u64 };
    // 1 000 000 x 1 MiB = about 1 TB: Vec::with_capacity of it aborts the process
    let pieces: Vec<Piece> = (0..1_000_000).map(|_| Piece::Source(whole)).collect();
    let e = stitch(&src, &plan(pieces), &cx2x2()).unwrap_err();
    assert!(e.contains("longer than"), "{e}");
    // a repeat of a small file stays inside the allowance
    let ok = stitch(&src, &plan(vec![Piece::Source(whole), Piece::Source(whole)]), &cx2x2()).unwrap();
    assert_eq!(ok.len(), 2 << 20);
}

#[test]
fn a_plan_with_too_many_pieces_is_refused() {
    let pieces: Vec<Piece> = (0..MAX_PLAN_PIECES + 1).map(|_| Piece::Source(Span { offset: 0, len: 0 })).collect();
    let e = stitch(b"abc", &plan(pieces), &cx2x2()).unwrap_err();
    assert!(e.contains("pieces"), "{e}");
}

// 2. transforms that announce a length

#[test]
fn an_announced_length_is_held_to_the_cap_before_anything_is_reserved() {
    let cx = ReadCx::new(None);
    let e = env(&cx);
    for c in [Codec::Lz4Block(u32::MAX), Codec::Lzf(u32::MAX), Codec::Packbits(u32::MAX)] {
        // the last step: the cap is the length the layout expects
        let err = transform::apply(&e, &[c], Cow::Borrowed(&[0u8, 1, 2]), Some(4)).unwrap_err();
        assert!(format!("{err:?}").contains("announces"), "{c:?}: {err:?}");
        // a step that is not the last: the cap is the 1 GiB of every transform
        let err = transform::apply(&e, &[c, Codec::Zlib], Cow::Borrowed(&[0u8, 1, 2]), None).unwrap_err();
        assert!(format!("{err:?}").contains("announces"), "{c:?}: {err:?}");
    }
    // inside the cap but far beyond what 2 bytes can make: it fails as short, having reserved little
    let err = transform::lz4_block(&[0x10, 1], 1 << 30).unwrap_err();
    assert!(format!("{err:?}").contains("less output"), "{err:?}");
}

#[test]
fn tiles_of_one_pixel_cannot_make_the_host_reserve_gigabytes() {
    let cx = ReadCx::new(None);
    let tile = |c: Codec| Tile {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
        content: TileContent::Raster(Raster { data: Bytes::Inline(vec![0, 1, 2]), codecs: vec![c], layout: rgba_layout(0) }),
    };
    for c in [Codec::Lz4Block(u32::MAX), Codec::Lzf(u32::MAX), Codec::Packbits(u32::MAX)] {
        let b = Bitmap { size: Some((1, 1)), source: PixelSource::Tiles(Tiled { tiles: (0..64).map(|_| tile(c)).collect(), overlap: Overlap::Replace }) };
        assert!(decode_bitmap(&env(&cx), &b).is_err());
    }
}

// 3. pixels.read and encode-tile

#[test]
fn a_row_padding_of_four_gigabytes_is_refused_before_the_output_is_made() {
    let mut cx = cx2x2();
    let e = cx.read(0, 0, 0, 1, 1, &rgba_layout(u32::MAX)).unwrap_err();
    assert!(e.contains("over"), "{e}");
    let e = cx.encode_tile(0, 0, 0, 2, 2, &rgba_layout(u32::MAX), &[Encoding::Raw]).unwrap_err();
    assert!(e.contains("over"), "{e}");
    // an ordinary padding still reads
    assert_eq!(cx.read(0, 0, 0, 2, 2, &rgba_layout(3)).unwrap().len(), 2 * (8 + 3));
}

// 4. the pixels the host makes have a byte budget

#[test]
fn the_pixels_made_in_one_write_have_a_budget() {
    let mut cx = cx2x2().with_budgets(16 * 10, 1 << 20);
    let mut made = 0;
    let err = loop {
        match cx.thumbnail(0, 100) {
            Ok(_) => made += 1,
            Err(e) => break e,
        }
        assert!(made < 100, "no budget");
    };
    assert_eq!(made, 10);
    assert!(err.contains("over"), "{err}");
}

// 5. raw pixels whose size wraps

#[test]
fn raw_pixels_of_a_wrapping_size_are_refused() {
    let raw = RawPixels { x: 0, y: 0, width: 1 << 31, height: 1 << 31, rgba: vec![] };
    let b = Bitmap { size: None, source: PixelSource::Raw(raw) };
    let cx = ReadCx::new(None);
    assert!(decode_bitmap(&env(&cx), &b).is_err(), "decode panicked or passed");
    let s = Structure {
        format: "t".into(),
        canvas: Canvas { width: 1, height: 1, dpi: None, profile: ColorProfile::None },
        layers: vec![Layer {
            id: "a".into(),
            stable_id: "a".into(),
            parent: None,
            name: "a".into(),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            clip_to_below: false,
            offset_x: 0,
            offset_y: 0,
            pixels: LayerPixels::Ready(b),
            meta: vec![],
            semantic: vec![],
            write: WriteCapability::None,
        }],
        composite: Composite::None,
        file_meta: vec![],
        context: vec![],
        warnings: vec![],
    };
    assert!(format!("{:?}", validate::check(&s, 0).unwrap_err()).contains("raw pixels"));
    // the exact size passes
    let ok = Bitmap { size: None, source: PixelSource::Raw(RawPixels { x: 0, y: 0, width: 2, height: 1, rgba: vec![0; 8] }) };
    assert_eq!(decode_bitmap(&env(&cx), &ok).unwrap().data.len(), 8);
}
