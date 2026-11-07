//! What a format plugin said about a file, kept (PLUGINS.md 6.2, P9): its
//! `probe` verdict, its `read-structure` and the recipes its `read-layer`
//! gave for deferred layers. A plugin is a pure function of the component
//! and the file's bytes (PLUGINS.md 3), so the same component reading the
//! same bytes again would say the same: the next import of those bytes
//! (`status` and `diff` of a modified working file, run again and again;
//! the `add` after them) skips those calls. The pipeline still checks the
//! structure (`validate::check`) and decodes, digests and records the
//! pixels as before, so no record changes.
//!
//! - On disk: `.tmi/cache/plugin/<component_sha256>/<source_digest>`, the
//!   CBOR of `tmiki plugin dev`'s values (`tmi_plugin_host::devwire`, the one
//!   mapping of the WIT's values) under the cache's own format name and the
//!   component's ABI; anything that does not decode to that is a miss. `gc`
//!   empties `.tmi/cache` (with the render cache). Written after an import
//!   that succeeded, by a temporary file and a rename, when the plugin's
//!   answers took at least [`MIN_COST`] (else asking again is as quick).
//! - In memory, once: what [`StructureCache::prefetch`] read on its own
//!   thread for the next file of a bulk `add` while the current one
//!   decodes (PLUGINS.md 6.1), taken by that file's import.
//! - Never for a development plugin (`component_sha256 = "dev"` for every
//!   build), and not with `TMI_NO_STRUCTURE_CACHE=1`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tmi_format::pipeline::{self, ImportError, Importer};
use tmi_import::{ImportedFile, Options, Warnings};
use tmi_format::reader::{FormatReader, ReadCx, Source};
use tmi_format::types::*;
use tmi_plugin_dev::Value as Cbor;
use tmi_plugin_host::devwire;

/// The format of a cache file (a new one is a miss for the old files).
pub const FORMAT: &str = "tmi-plugin-structure 1";

/// A cache file larger than this is not written (a plugin may put pixels
/// inline in its recipes).
const MAX_FILE: usize = 64 << 20;

/// The environment variable that turns the cache off.
pub const OFF_ENV: &str = "TMI_NO_STRUCTURE_CACHE";

/// What a plugin's answers must have cost before they are written to a
/// cache file: below it, writing and reading the file costs about as much
/// as asking again (a Rust plugin's small file: a write is about 0.5 ms on
/// Windows). `TMI_STRUCTURE_CACHE_MIN_MS` sets it (tests: 0).
const MIN_COST: Duration = Duration::from_millis(2);

fn min_cost() -> Duration {
    std::env::var("TMI_STRUCTURE_CACHE_MIN_MS").ok().and_then(|v| v.trim().parse().ok()).map_or(MIN_COST, Duration::from_millis)
}

/// What a plugin said about one file.
#[derive(Clone, Debug)]
pub struct Said {
    pub probe: Confidence,
    pub structure: Structure,
    /// The recipes of deferred layers, by token (those read so far).
    pub layers: HashMap<u32, Bitmap>,
    /// Read from the cache file (else from a prefetch: not on disk yet).
    pub on_disk: bool,
    /// How long the plugin took to say it (in this process).
    pub cost: Duration,
}

impl Said {
    fn encode(&self, abi: &str) -> Vec<u8> {
        let mut tokens: Vec<&u32> = self.layers.keys().collect();
        tokens.sort();
        Cbor::record([
            ("format", Cbor::text(FORMAT)),
            ("abi", Cbor::text(abi)),
            ("probe", devwire::confidence_value(self.probe)),
            ("structure", devwire::structure_value(&self.structure)),
            ("layers", Cbor::list(tokens, |t| Cbor::Array(vec![Cbor::uint(*t), devwire::bitmap_value(&self.layers[t])]))),
        ])
        .encode()
    }

    fn decode(data: &[u8], abi: &str) -> Option<Said> {
        let v = Cbor::decode(data).ok()?;
        if v.field("cache", "format").ok()?.str("format").ok()? != FORMAT || v.field("cache", "abi").ok()?.str("abi").ok()? != abi {
            return None;
        }
        let probe = devwire::confidence(v.field("cache", "probe").ok()?, "probe").ok()?;
        let structure = devwire::structure(v.field("cache", "structure").ok()?).ok()?;
        let mut layers = HashMap::new();
        for item in v.field("cache", "layers").ok()?.array("layers").ok()? {
            let pair = item.array("layer").ok()?;
            let [token, bitmap] = pair else { return None };
            layers.insert(token.u32("token").ok()?, devwire::bitmap(bitmap, "layer").ok()?);
        }
        Some(Said { probe, structure, layers, on_disk: true, cost: Duration::ZERO })
    }
}

type Key = (String, String);

/// A prefetch in flight: None until it ends, then what it read (None: it
/// failed; the import calls the plugin itself and meets the error).
type Pending = Arc<(Mutex<Option<Option<Said>>>, Condvar)>;

/// One command's cache (held by `Formats`).
pub struct StructureCache {
    dir: Option<PathBuf>,
    pending: Mutex<HashMap<Key, Pending>>,
}

/// Whether `importer`'s structures may be kept at all.
pub fn usable(importer: &Importer) -> bool {
    !importer.is_dev() && !off()
}

fn off() -> bool {
    let v = std::env::var(OFF_ENV).unwrap_or_default().trim().to_lowercase();
    ["1", "true", "yes", "on"].contains(&v.as_str())
}

impl StructureCache {
    /// `dir`: `.tmi/cache/plugin` (None: memory only).
    pub fn new(dir: Option<PathBuf>) -> StructureCache {
        StructureCache { dir, pending: Mutex::new(HashMap::new()) }
    }

    fn path(&self, component: &str, digest: &str) -> Option<PathBuf> {
        // both are hex digests; anything else never names a file
        let hex = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex(component) || !hex(digest) {
            return None;
        }
        Some(self.dir.as_ref()?.join(component).join(digest))
    }

    /// What `importer`'s component said about the bytes of `digest`: a
    /// prefetch's (waited for), else the file's.
    pub fn get(&self, importer: &Importer, digest: &str) -> Option<Said> {
        let key = (importer.component_sha256.clone(), digest.to_string());
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(p) = pending {
            let (lock, cv) = &*p;
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            while g.is_none() {
                g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            if let Some(Some(said)) = g.take() {
                return Some(said);
            }
        }
        self.load(importer, digest)
    }

    fn load(&self, importer: &Importer, digest: &str) -> Option<Said> {
        let data = std::fs::read(self.path(&importer.component_sha256, digest)?).ok()?;
        Said::decode(&data, &importer.abi)
    }

    /// Keeps `said` for the bytes of `digest` (best effort: a cache that
    /// cannot be written is no error).
    pub fn put(&self, importer: &Importer, digest: &str, said: &Said) {
        let Some(path) = self.path(&importer.component_sha256, digest) else { return };
        let data = said.encode(&importer.abi);
        if data.len() > MAX_FILE {
            return;
        }
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = dir.join(format!("{digest}.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, &data).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// `pipeline::import_plugin` through what was kept for these bytes
    /// (keeping what is new when the import succeeds); as it is for a
    /// development plugin or with the cache off.
    #[allow(clippy::too_many_arguments)]
    pub fn import_plugin(
        &self,
        reader: &dyn FormatReader,
        importer: &Importer,
        name: &str,
        bytes: &[u8],
        digest: &str,
        opts: &Options,
        warns: &mut Warnings,
    ) -> Result<ImportedFile, ImportError> {
        if !usable(importer) {
            return pipeline::import_plugin(reader, importer, name, bytes, digest, opts, warns);
        }
        let caching = CachingReader::new(reader, self.get(importer, digest));
        let r = pipeline::import_plugin(&caching, importer, name, bytes, digest, opts, warns);
        if r.is_ok() {
            if let Some(new) = caching.into_new().filter(|n| n.on_disk || n.cost >= min_cost()) {
                self.put(importer, digest, &new);
            }
        }
        r
    }

    /// Starts reading the structure of the file `name` (`bytes`, sha256
    /// `digest`) with `reader` on a thread of its own, unless the cache
    /// file already holds it: the next `get` of it waits for that read. The
    /// calls run on the plugin's own workers (PLUGINS.md 6.1), never on
    /// rayon's threads, and take nothing the current import holds.
    pub fn prefetch(&self, reader: Arc<dyn FormatReader>, importer: &Importer, name: String, bytes: Arc<Vec<u8>>, digest: &str) {
        let key = (importer.component_sha256.clone(), digest.to_string());
        if self.path(&key.0, &key.1).is_some_and(|p| p.is_file()) {
            return;
        }
        let pending: Pending = Arc::new((Mutex::new(None), Condvar::new()));
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if map.contains_key(&key) {
                return;
            }
            map.insert(key, Arc::clone(&pending));
        }
        let spawned = std::thread::Builder::new().name("tmi-structure-prefetch".into()).spawn({
            let pending = Arc::clone(&pending);
            move || {
                let read = || -> Option<Said> {
                    let start = Instant::now();
                    let head = &bytes[..bytes.len().min(reader.info().probe_bytes as usize)];
                    let probe = reader.probe(&name, head, bytes.len() as u64);
                    if probe == Confidence::No {
                        return None;
                    }
                    let structure = reader.read_structure(&Source::new(&name, &bytes), &ReadCx::new(None)).ok()?;
                    Some(Said { probe, structure, layers: HashMap::new(), on_disk: false, cost: start.elapsed() })
                };
                let said = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).ok().flatten();
                let (lock, cv) = &*pending;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(said);
                cv.notify_all();
            }
        });
        if spawned.is_err() {
            let (lock, cv) = &*pending;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(None);
            cv.notify_all();
        }
    }
}

/// A plugin's reader that answers from what was kept and keeps what the
/// plugin says.
pub struct CachingReader<'a> {
    inner: &'a dyn FormatReader,
    prior: Option<Said>,
    probe: Mutex<Option<Confidence>>,
    structure: Mutex<Option<Structure>>,
    layers: Mutex<HashMap<u32, Bitmap>>,
    /// The time spent in the plugin's calls.
    spent: Mutex<Duration>,
}

impl<'a> CachingReader<'a> {
    pub fn new(inner: &'a dyn FormatReader, prior: Option<Said>) -> CachingReader<'a> {
        CachingReader { inner, prior, probe: Mutex::new(None), structure: Mutex::new(None), layers: Mutex::new(HashMap::new()), spent: Mutex::new(Duration::ZERO) }
    }

    fn timed<T>(&self, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let r = f();
        *self.spent.lock().unwrap_or_else(|e| e.into_inner()) += start.elapsed();
        r
    }

    /// What the plugin said, kept and new together, when there is anything
    /// new to keep.
    pub fn into_new(self) -> Option<Said> {
        let layers = self.layers.into_inner().unwrap_or_else(|e| e.into_inner());
        let spent = self.spent.into_inner().unwrap_or_else(|e| e.into_inner());
        match self.prior {
            Some(mut said) => {
                if layers.is_empty() && said.on_disk {
                    return None;
                }
                said.layers.extend(layers);
                said.cost += spent;
                Some(said)
            }
            None => {
                let probe = self.probe.into_inner().unwrap_or_else(|e| e.into_inner())?;
                let structure = self.structure.into_inner().unwrap_or_else(|e| e.into_inner())?;
                Some(Said { probe, structure, layers, on_disk: false, cost: spent })
            }
        }
    }
}

impl FormatReader for CachingReader<'_> {
    fn info(&self) -> &PluginInfo {
        self.inner.info()
    }

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Confidence {
        if let Some(p) = &self.prior {
            return p.probe;
        }
        let c = self.timed(|| self.inner.probe(name, head, size));
        *self.probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
        c
    }

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError> {
        if let Some(p) = &self.prior {
            return Ok(p.structure.clone());
        }
        let s = self.timed(|| self.inner.read_structure(src, cx))?;
        *self.structure.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
        Ok(s)
    }

    fn read_layer(&self, src: &Source, context: &[u8], token: u32, cx: &ReadCx) -> Result<Bitmap, ReadError> {
        if let Some(b) = self.prior.as_ref().and_then(|p| p.layers.get(&token)) {
            return Ok(b.clone());
        }
        let b = self.timed(|| self.inner.read_layer(src, context, token, cx))?;
        self.layers.lock().unwrap_or_else(|e| e.into_inner()).insert(token, b.clone());
        Ok(b)
    }

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, ReadError> {
        self.inner.decode_chunk(codec_id, input, out_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(c: u8) -> Bitmap {
        Bitmap {
            size: Some((2, 1)),
            source: PixelSource::Tiles(Tiled {
                tiles: vec![Tile { x: 0, y: 0, width: 2, height: 1, content: TileContent::Fill(Rgba8 { r: c, g: 2, b: 3, a: 255 }) }],
                overlap: Overlap::Replace,
            }),
        }
    }

    fn said() -> Said {
        let layer = |id: &str, pixels| Layer {
            id: id.into(),
            stable_id: format!("s-{id}"),
            parent: None,
            name: format!("ein Name {id} 名前"),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 0.3,
            blend: BlendMode::Multiply,
            clip_to_below: false,
            offset_x: -7,
            offset_y: 9,
            pixels,
            meta: vec![MetaEntry { key: "k".into(), value: MetaValue::Number(0.1) }],
            semantic: vec![],
            write: WriteCapability::Pixels,
        };
        Said {
            probe: Confidence::Yes,
            structure: Structure {
                format: "X".into(),
                canvas: Canvas { width: 4, height: 3, dpi: Some(72.5), profile: ColorProfile::None },
                layers: vec![layer("a", LayerPixels::Ready(bitmap(1))), layer("b", LayerPixels::Deferred(5))],
                composite: Composite::Render(Compositor::GenericV1),
                file_meta: vec![],
                context: vec![0, 1, 2],
                warnings: vec![Warning { code: "w".into(), message: "m".into() }],
            },
            layers: HashMap::from([(5, bitmap(9))]),
            on_disk: false,
            cost: Duration::ZERO,
        }
    }

    #[test]
    fn a_kept_structure_reads_back_the_same_and_only_for_its_abi() {
        let s = said();
        let data = s.encode("tmi:format@0.1.0");
        let back = Said::decode(&data, "tmi:format@0.1.0").expect("decodes");
        assert_eq!(format!("{:?}", back.structure), format!("{:?}", s.structure));
        assert_eq!(format!("{:?}", back.layers), format!("{:?}", s.layers));
        assert_eq!(back.probe, Confidence::Yes);
        assert!(Said::decode(&data, "tmi:format@1.0.0").is_none());
        assert!(Said::decode(&data[..data.len() - 1], "tmi:format@0.1.0").is_none());
        assert!(Said::decode(b"", "tmi:format@0.1.0").is_none());
    }

    /// A plugin that counts what it is asked.
    struct Counting {
        info: PluginInfo,
        said: Said,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FormatReader for Counting {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
            self.calls.lock().unwrap().push("probe");
            Confidence::Maybe
        }
        fn read_structure(&self, _: &Source, _: &ReadCx) -> Result<Structure, ReadError> {
            self.calls.lock().unwrap().push("read-structure");
            std::thread::sleep(MIN_COST); // worth a cache file
            Ok(self.said.structure.clone())
        }
        fn read_layer(&self, _: &Source, context: &[u8], token: u32, _: &ReadCx) -> Result<Bitmap, ReadError> {
            assert_eq!(context, [0, 1, 2]);
            self.calls.lock().unwrap().push("read-layer");
            Ok(self.said.layers[&token].clone())
        }
    }

    fn record(f: &ImportedFile) -> String {
        let layers: Vec<String> = f.layers.iter().map(|l| format!("{:?} {:?} {:?}", l.pixel_sha256, l.pixel_bbox, l.source_meta)).collect();
        format!("{layers:?} {:?} {:?} {:?}", f.composite_png, f.canvas_meta, f.importer)
    }

    #[test]
    fn the_second_import_of_the_same_bytes_asks_the_plugin_nothing_and_records_the_same() {
        let dir = std::env::temp_dir().join(format!("tmi-structure-cache-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plugin = Counting {
            info: PluginInfo { id: "x".into(), version: "1".into(), formats: vec![], probe_bytes: 4, can_write: false },
            said: said(),
            calls: Mutex::new(Vec::new()),
        };
        let importer = |c: &str| Importer { id: "x".into(), version: "1".into(), component_sha256: c.into(), abi: "tmi:format@0.1.0".into() };
        let blobs = tmi_import::LayerBlobs::All;
        let opts = Options {
            render_composite: true,
            cache: None,
            reuse_pixels: None,
            reuse_decoded: None,
            layer_blobs: &blobs,
            composite_base: None,
            limits: tmi_import::Limits::NONE,
            verify_partial: false,
            progress: None,
        };
        let run = |cache: &StructureCache, c: &str| {
            let mut warns = Warnings::default();
            let f = cache.import_plugin(&plugin, &importer(c), "a.x", b"bytes", "dd", &opts, &mut warns).unwrap();
            (record(&f), warns.lines)
        };
        let calls = || std::mem::take(&mut *plugin.calls.lock().unwrap());
        let first = run(&StructureCache::new(Some(dir.clone())), "c1");
        assert_eq!(calls(), ["probe", "read-structure", "read-layer"]);
        assert!(dir.join("c1").join("dd").is_file());
        // another command, the same bytes: the file answers
        let again = run(&StructureCache::new(Some(dir.clone())), "c1");
        assert_eq!(calls(), Vec::<&str>::new());
        assert_eq!(again, first, "the same record and the same warnings");
        // another component reads them itself
        run(&StructureCache::new(Some(dir.clone())), "c2");
        assert_eq!(calls(), ["probe", "read-structure", "read-layer"]);
        // a damaged file is a miss
        std::fs::write(dir.join("c1").join("dd"), b"\xa1").unwrap();
        assert_eq!(run(&StructureCache::new(Some(dir.clone())), "c1"), first);
        assert_eq!(calls(), ["probe", "read-structure", "read-layer"]);
        // a prefetch is taken by the import
        let cache = StructureCache::new(None);
        let shared: Arc<dyn FormatReader> = Arc::new(Counting { info: plugin.info.clone(), said: said(), calls: Mutex::new(Vec::new()) });
        cache.prefetch(Arc::clone(&shared), &importer("c1"), "a.x".into(), Arc::new(b"bytes".to_vec()), "dd");
        assert_eq!(run(&cache, "c1"), first);
        assert_eq!(calls(), ["read-layer"], "the structure came from the prefetch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_named_by_component_and_digest() {
        let dir = std::env::temp_dir().join(format!("tmi-structure-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = StructureCache::new(Some(dir.clone()));
        let imp = |c: &str| Importer { id: "x".into(), version: "1".into(), component_sha256: c.into(), abi: "tmi:format@0.1.0".into() };
        cache.put(&imp("ab12"), "cd34", &said());
        assert!(dir.join("ab12").join("cd34").is_file());
        assert!(cache.get(&imp("ab12"), "cd34").is_some());
        assert!(cache.get(&imp("ab13"), "cd34").is_none(), "another component");
        assert!(cache.get(&imp("ab12"), "cd35").is_none(), "other bytes");
        cache.put(&imp("dev"), "cd34", &said());
        cache.put(&imp("ab12"), "../x", &said());
        assert!(!dir.join("dev").exists() && !dir.join("x").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
