//! `tmiki plugin dev` (P8): the ORA plugin as a development plugin (`ora-dev`,
//! the same code on tmi-plugin-sdk) gives the records and writes the bytes
//! its component gives, and a development plugin that misbehaves ends in a
//! defined error without tmiki panicking.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tmi_format::reader::FormatReader;
use tmi_format::write::{self, Edit, WriteFailure};
use tmi_import::{Limits, Rgba, Warnings};
use tmi_plugin_dev::Value;

use crate::dev::{self, DevCommand, DevOptions, DevProcessBackend};
use crate::ora_files::{self, Merged, Sample};
use crate::reader::BackendReader;
use crate::tests::{import_both, plugin, samples};
use crate::{test_plugins, PluginBackend, PluginError, WasmReader};

fn ora_dev(timeout: Duration) -> BackendReader<DevProcessBackend> {
    dev::open(DevCommand::new(test_plugins::ora_dev(), Vec::<String>::new()), DevOptions { timeout: Some(timeout), ..DevOptions::default() }).unwrap()
}

fn shared() -> &'static BackendReader<DevProcessBackend> {
    static DEV: OnceLock<BackendReader<DevProcessBackend>> = OnceLock::new();
    DEV.get_or_init(|| ora_dev(Duration::from_secs(60)))
}

#[test]
fn the_development_plugin_says_what_its_component_says() {
    let dev = shared();
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    assert_eq!(dev.info(), wasm.info());
    assert_eq!(dev.backend().abi(), "tmi:format@0.1.0");
    assert!(dev.backend().can_write());
    let importer = dev.backend().importer();
    assert_eq!((importer.component_sha256.as_str(), importer.id.as_str()), ("dev", "ora"));
}

#[test]
fn the_development_plugin_makes_the_records_its_component_makes() {
    let dev = shared();
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    for s in samples() {
        let data = ora_files::sample(s);
        let w = import_both(&wasm, "doc.ora", &data).unwrap_or_else(|e| panic!("{s:?}: {e:?}"));
        let d = import_both(dev, "doc.ora", &data).unwrap_or_else(|e| panic!("{s:?}: {e:?}"));
        assert_eq!(w, d, "{s:?}");
    }
    let data = ora_files::many_layers(200, 16, true, true);
    assert_eq!(import_both(&wasm, "many.ora", &data).unwrap(), import_both(dev, "many.ora", &data).unwrap());
}

/// The values `tmiki plugin test` compares in are what tmiki reads back from a
/// development plugin: `devwire`'s `*_value` and its decoders are inverses.
#[test]
fn the_values_test_compares_are_the_rpc_values() {
    use tmi_format::reader::{ReadCx, Source};
    use tmi_format::types::{LayerPixels, ReadError};
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    let mut cases = samples().into_iter().map(ora_files::sample).collect::<Vec<_>>();
    cases.push(ora_files::many_layers(12, 16, true, true));
    for data in cases {
        let src = Source::new("doc.ora", &data);
        let cx = ReadCx::new(None);
        let mut s = wasm.read_structure(&src, &cx).unwrap();
        let v = crate::devwire::structure_value(&s);
        assert_eq!(crate::devwire::structure(&Value::decode(&v.encode()).unwrap()).unwrap(), s);
        for l in s.layers.iter_mut() {
            if let LayerPixels::Deferred(t) = l.pixels {
                l.pixels = LayerPixels::Ready(wasm.read_layer(&src, &s.context, t, &cx).unwrap());
            }
        }
        assert_eq!(crate::devwire::structure(&crate::devwire::structure_value(&s)).unwrap(), s);
    }
    let info = wasm.info();
    assert_eq!(&crate::devwire::plugin_info(&crate::devwire::plugin_info_value(info)).unwrap(), info);
    for e in [ReadError::Corrupt("x".into()), ReadError::NotThisFormat("y".into()), ReadError::Unsupported("z".into()), ReadError::TooLarge("w".into())] {
        assert_eq!(crate::devwire::read_error(&crate::devwire::read_error_value(&e)).unwrap(), e);
    }
}

fn new_top() -> Rgba {
    Rgba { width: 20, height: 12, data: ora_files::pattern(20, 12, 77) }
}

fn write_with(reader: &impl tmi_format::reader::FormatWriter, data: &[u8]) -> Result<Vec<u8>, WriteFailure> {
    let importer = crate::tests::importer(reader);
    write::write_checked(reader, &importer, "doc.ora", data, vec![Edit { stable_id: "ora:3".into(), pixels: new_top() }], Limits::NONE, &mut Warnings::default())
}

#[test]
fn the_development_plugin_writes_the_bytes_its_component_writes() {
    let dev = shared();
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    for deflate in [false, true] {
        for merged in [Merged::None, Merged::Canvas] {
            for defer in [false, true] {
                let s = Sample { deflate, merged, defer };
                let data = ora_files::sample(s);
                let w = write_with(&wasm, &data).unwrap_or_else(|e| panic!("{s:?}: {e}"));
                let d = write_with(dev, &data).unwrap_or_else(|e| panic!("{s:?}: {e}"));
                assert!(w == d, "{s:?}: the development plugin wrote other bytes");
            }
        }
    }
    // and its faults are caught as the component's are
    for (fault, want) in [("other-layer", "changed although it was not edited"), ("bad-span", "is outside the")] {
        let data = ora_files::sample_with(Sample { deflate: true, merged: Merged::Canvas, defer: false }, &format!(r#" tmi-test-write-fault="{fault}""#));
        let e = write_with(dev, &data).unwrap_err();
        assert!(e.to_string().contains(want), "{fault}: {e}");
    }
}

fn fault(what: &str) -> Vec<u8> {
    ora_files::sample_with(Sample { deflate: false, merged: Merged::None, defer: false }, &format!(r#" tmi-test-dev-fault="{what}""#))
}

#[test]
fn a_development_plugin_that_misbehaves_is_an_error_and_the_next_call_works() {
    let dev = ora_dev(Duration::from_secs(3));
    let good = ora_files::sample(Sample { deflate: true, merged: Merged::None, defer: false });
    let b = dev.backend();
    let trap = |r: Result<tmi_format::types::Structure, PluginError>| match r {
        Err(PluginError::Trap(m)) => m,
        other => panic!("{other:?}"),
    };
    let m = trap(b.read_structure("doc.ora", &fault("panic")));
    assert!(m.contains("panic: the test fault asked for a panic"), "{m}");
    // the process is still the same one: a panic is the call's trap
    assert!(b.read_structure("doc.ora", &good).is_ok());
    let m = trap(b.read_structure("doc.ora", &fault("bad-handle")));
    assert!(m.starts_with("codecs.crc32: "), "{m}");
    let m = trap(b.read_structure("doc.ora", &fault("exit")));
    assert!(m.contains("exited with code 3"), "{m}");
    assert!(b.read_structure("doc.ora", &good).is_ok(), "started again");
    let m = trap(b.read_structure("doc.ora", &fault("stdout")));
    assert!(m.contains("did the plugin print to stdout?"), "{m}");
    let t = Instant::now();
    match b.read_structure("doc.ora", &fault("hang")) {
        Err(PluginError::Timeout { limit }) => assert_eq!(limit, Duration::from_secs(3)),
        other => panic!("{other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(b.read_structure("doc.ora", &good).is_ok(), "started again after the time limit");
    // through the reader, a failure is the host's verdict, never not-this-format
    let r = ora_dev(Duration::from_secs(3));
    match r.read_structure(&tmi_format::reader::Source::new("doc.ora", &fault("panic")), &tmi_format::reader::ReadCx::new(None)) {
        Err(tmi_format::types::ReadError::Plugin(m)) => assert!(m.starts_with("plugin ora 0.3.0: crashed (panic: "), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_program_that_is_not_a_development_plugin_is_refused() {
    let missing = DevProcessBackend::start(DevCommand::new("tmi-no-such-program-xyz", Vec::<String>::new()), DevOptions::default());
    assert!(matches!(missing, Err(PluginError::Compile(ref m)) if m.contains("cannot run the development plugin")), "{:?}", missing.err());
    // cargo prints its version on stdout: not a frame
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let not = DevProcessBackend::start(DevCommand::new(cargo, ["--version"]), DevOptions { timeout: Some(Duration::from_secs(20)), ..DevOptions::default() });
    match not {
        Err(PluginError::Trap(m)) => assert!(m.contains("did the plugin print to stdout?"), "{m}"),
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn the_shared_fixture_reads_as_the_sdk_wrote_it() {
    use tmi_format::types::*;
    let bytes = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tmi-plugin-dev/testdata/structure.cbor")).unwrap();
    let s = crate::devwire::structure(&Value::decode(&bytes).unwrap()).unwrap();
    assert_eq!(s.format, "Kitchen sink");
    assert_eq!(s.canvas, Canvas { width: 256, height: 129, dpi: Some(350.5), profile: ColorProfile::Embedded(Bytes::Span(Span { offset: 1, len: 2 })) });
    assert_eq!(s.layers.len(), 30);
    // every blend mode, in the WIT's order, which is tmiki's
    let blends: Vec<&str> = s.layers.iter().map(|l| l.blend.name()).collect();
    assert_eq!(blends[..4], ["normal", "pass_through", "multiply", "screen"]);
    assert_eq!(blends[29], "other");
    assert_eq!(s.layers[9].kind, LayerKind::Unknown);
    assert_eq!(s.layers[2].write, WriteCapability::PixelsAndProps);
    assert_eq!((s.layers[3].offset_x, s.layers[3].offset_y), (-3000, i64::MAX - 3));
    assert_eq!(s.layers[1].pixels, LayerPixels::Deferred(1));
    assert_eq!(s.layers[0].parent, None);
    assert_eq!(s.layers[1].meta.len(), 7);
    assert_eq!(s.layers[1].meta[5].value, MetaValue::Sha256Of(Bytes::Inline(vec![1, 2, 3])));
    assert_eq!(s.layers[2].semantic[4].value, MetaValue::Text("名前".into()));
    let LayerPixels::Ready(Bitmap { size: Some((256, 128)), source: PixelSource::Tiles(t) }) = &s.layers[2].pixels else { panic!() };
    assert_eq!(t.overlap, Overlap::Over);
    let TileContent::Raster(r) = &t.tiles[0].content else { panic!() };
    assert_eq!(r.codecs.len(), 10);
    assert_eq!(r.codecs[7], Codec::Predictor(Predictor { kind: PredictorKind::TiffHorizontal, row_bytes: 64, bytes_per_pixel: 4 }));
    assert_eq!(r.codecs[9], Codec::Plugin(7));
    assert_eq!(r.layout.groups[1].channels, [Channel::B, Channel::G, Channel::R, Channel::Pad]);
    assert_eq!(r.layout.groups.iter().map(|g| g.depth).collect::<Vec<_>>(), [Depth::Eight, Depth::SixteenBe, Depth::OneBitMsb, Depth::SixteenLe, Depth::Float32Le]);
    assert_eq!(r.layout.tint, Some(Rgba8 { r: 1, g: 2, b: 3, a: 255 }));
    assert_eq!(t.tiles[0].x, -5);
    assert_eq!(t.tiles[2].content, TileContent::Fill(Rgba8 { r: 9, g: 8, b: 7, a: 6 }));
    assert_eq!(t.tiles[3].content, TileContent::Empty);
    let LayerPixels::Ready(Bitmap { source: PixelSource::Image(p), .. }) = &s.layers[3].pixels else { panic!() };
    assert_eq!((p.x, p.y, p.image.codec), (3, -4, ImageCodec::Gif));
    let codecs: Vec<ImageCodec> = (0..30).filter(|i| i % 6 == 3).map(|i| match &s.layers[i].pixels {
        LayerPixels::Ready(Bitmap { source: PixelSource::Image(p), .. }) => p.image.codec,
        _ => panic!(),
    }).collect();
    assert_eq!(codecs, [ImageCodec::Gif, ImageCodec::WebpV1, ImageCodec::JpegV1, ImageCodec::Png, ImageCodec::Qoi]);
    let LayerPixels::Ready(Bitmap { source: PixelSource::Raw(raw), .. }) = &s.layers[4].pixels else { panic!() };
    assert_eq!(raw.rgba, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert!(matches!(&s.layers[5].pixels, LayerPixels::Ready(Bitmap { size: Some((4, 4)), source: PixelSource::Empty })));
    assert!(matches!(s.composite, Composite::Image(_)));
    assert_eq!(s.context, [0, 255, 7]);
    assert_eq!(s.warnings, [Warning { code: "c".into(), message: "m".into() }]);
}
