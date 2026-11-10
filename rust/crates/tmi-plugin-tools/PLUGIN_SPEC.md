# PLUGIN_SPEC.md — tmi の形式プラグインの仕様（tmi:format@0.1.0）

tmi（イラストのファイル向けのバージョン管理）に新しいファイル形式を足す「形式プラグイン」の作り方の全部である。この文書とサンプルのファイルがあれば、プラグインを書き、`tmiki plugin test` を通すところまで進められるように書いてある。人が読んでも、AI に渡してもよい。`tmiki plugin new` がプラグインのディレクトリに書き出す（tmi の版と同じ内容）。

- 対象のインターフェース：`tmi:format@0.1.0`（実験版）。`wit/tmi-format.wit` に全文がある（1.1 にも同じものを載せた）。1.0 になるまで型が変わることがあり、そのときは tmi が記録を取り込み直す手順を出す。
- 言語：**Rust**。プラグインは Rust で書き、`wasm32-wasip2` の WebAssembly の component（world `format-reader` か `format-plugin`）にする。tmi が雛形（`tmiki plugin new`）・SDK（wit-bindgen 0.63 の上の `tmi-plugin-sdk`）・`tmiki plugin build` を持つのは Rust だけで、ほかの言語で作った component は大きすぎて tmi が読み込まない（Rust の component は速く小さい：インスタンスの作成 50 µs ほど、数百 KB）。同じコードを開発用プラグインとしても動かせる（11 節）。
- 用語：「ホスト」は tmi、「プラグイン」はあなたが書く component。「記録」は tmi がリポジトリに保存するもの（レイヤーの木、各レイヤーの画素の SHA-256、レイヤーの blob、合成画像）。

## 0. 全体像

プラグインは**画素をデコードしない**。ファイルの目次を読み、各レイヤーの画素が「ファイルのどこに、どの形で入っているか」（recipe）を返す。展開・画像のデコード・合成・ハッシュはホストがネイティブのコードで並列に行う。

```
tmiki add foo.kra
  └ ホスト：probe(名前, 先頭 N バイト, 大きさ) ─────────► プラグイン：yes / maybe / no
  └ ホスト：read-structure(ファイル) ──────────────────► プラグイン：structure
        structure = キャンバス・レイヤーの列（木を平らにしたもの）・合成の選び方・警告
        各レイヤーの画素 = ready(recipe) か deferred(token) か none
  └ ホスト：read-layer(ファイル, context, token) ×N ──► プラグイン：recipe（deferred のぶん。並列）
  └ ホスト：recipe をデコード → 画素 → SHA-256・blob・合成・サムネイル
```

書き戻し（world `format-plugin`。マージの結果や `tmiki restore --in-place` をファイルに書く）では、ホストが新しい画素を渡し、プラグインは「新しいファイルを、元のファイルの範囲・自分のバイト列・ホストが符号化したバイト列をつなげて作る手順」（write-plan）を返す。ホストはつなげたものを同じプラグインで読み直し、編集したレイヤーだけが変わったことを確かめてから書く（6 節）。

プラグインはサンドボックスの中で動く。ファイルシステム・ネットワーク・時刻・乱数・環境変数には触れない（触ろうとしても空か固定の値が返る）。見えるのは取り込み中のファイル 1 つとその名前だけ。

## 1. インターフェース

### 1.1 WIT（`wit/tmi-format.wit` と同じもの）

```wit
// tmi:format 0.1.0 (experimental). PLUGINS.md section 2.1 is the design this
// file follows; where the two differ, PLUGINS.md "P1 の結果" says why.
package tmi:format@0.1.0;

/// Plain values the plugin returns. The host checks them, then decodes and
/// composites natively.
interface types {
  /// A part of the source file (absolute offset).
  record span { offset: u64, len: u64 }

  /// Bytes the host takes out of the file without going through the plugin.
  variant bytes {
    span(span),
    spans(list<span>),        // concatenation (a split stream)
    inline(list<u8>),         // made by the plugin (up to 64 MiB each)
  }

  /// A transform of a byte string, applied in the order listed. Each name has
  /// one fixed meaning (PLUGINS.md 3.3).
  variant codec {
    zlib,                     // RFC 1950 (adler32 is checked)
    deflate,                  // RFC 1951, raw
    gzip,
    zstd,
    lz4-block(u32),           // size after expansion
    lzf(u32),                 // Krita tiles
    packbits(u32),            // PSD / TIFF RLE; size after expansion
    predictor(predictor),     // undo a PNG row filter / the TIFF horizontal difference
    plugin(u32),              // the plugin's own codec: the host calls reader.decode-chunk
  }
  enum predictor-kind { png-rows, tiff-horizontal }
  record predictor { kind: predictor-kind, row-bytes: u32, bytes-per-pixel: u8 }

  /// An image that knows its own size and that the host decodes. A format
  /// whose output depends on the implementation carries a generation in its
  /// name (3.3).
  enum image-codec { png, jpeg-v1, webp-v1, gif, bmp, tiff, qoi }
  record encoded-image {
    data: bytes,
    pre: list<codec>,        // transforms stripped before the image (a zip's deflate)
    codec: image-codec,
    frame: u32,              // which frame of a multi-frame format
  }

  enum channel { r, g, b, a, gray, pad }
  enum depth { one-bit-msb, eight, sixteen-be, sixteen-le, float32-le }
  /// One run of pixels. A decoded tile is the groups in order, each holding
  /// every pixel of the tile (CLIP: [a], then [b, g, r, pad]).
  record group { channels: list<channel>, depth: depth }
  record rgba8 { r: u8, g: u8, b: u8, a: u8 }
  record layout {
    groups: list<group>,
    row-padding: u32,          // bytes after each row of each group (a 1-bit row is rounded up to whole bytes first)
    tint: option<rgba8>,       // no colour channel: rgb = tint.rgb. With a tint, a = a * tint.a / 255 (truncated)
    premultiplied: bool,       // the host undoes it (the formula of Pillow's RGBa to RGBA)
  }
  record raster { data: bytes, codecs: list<codec>, layout: layout }

  variant tile-content {
    raster(raster),            // width * height pixels after codecs
    image(encoded-image),      // must be the size of the tile
    fill(rgba8),
    empty,
  }
  /// x and y are positions in the bitmap; what sticks out is cut by the bitmap.
  record tile { x: s32, y: s32, width: u32, height: u32, content: tile-content }
  enum overlap {
    replace,                   // a later tile overwrites
    over,                      // a later tile is laid on with Pillow's alpha_composite (MDP2)
    reject,                    // an overlap fails the import
  }
  record tiled { tiles: list<tile>, overlap: overlap }
  record placed-image { x: s32, y: s32, image: encoded-image }
  /// Last resort: RGBA8 rows the plugin computed.
  record raw-pixels { x: s32, y: s32, width: u32, height: u32, rgba: list<u8> }

  variant pixel-source {
    empty,
    image(placed-image),
    tiles(tiled),
    raw(raw-pixels),
  }
  /// A layer's bitmap. A size of none is, for a single image only, that image's size.
  record bitmap { size: option<tuple<u32, u32>>, source: pixel-source }

  enum layer-kind { raster, folder, text, vector, adjustment, fill, paper, mask, stencil, unknown }
  /// The names of tmi-render/src/blend.rs (recorded in snake_case).
  enum blend-mode {
    normal, pass-through, multiply, screen, darken, lighten, difference, subtract,
    linear-dodge, linear-burn, color-dodge, color-burn, overlay, hard-light, soft-light,
    exclusion, divide, darker-color, lighter-color, hue, saturation, color, luminosity,
    hard-mix, linear-light, vivid-light, pin-light, glow-add, glow-dodge, other,
  }
  enum write-capability { none, pixels, pixels-and-props }

  variant meta-value {
    null, boolean(bool), integer(s64), number(f64), text(string),
    sha256-of(bytes),          // the host writes the SHA-256 in hex (no copy of a big blob)
    json(string),              // a nested value; the host rewrites it as canonical JSON
  }
  record meta-entry { key: string, value: meta-value }

  variant layer-pixels { none, ready(bitmap), deferred(u32) }

  record layer {
    id: string,                // layer_id
    stable-id: string,         // stable_source_id (unique in the file)
    parent: option<u32>,       // index into layers, smaller than this layer's own
    name: string,
    kind: layer-kind,
    visible: bool,
    opacity: f64,              // 0..1; the host rounds to 6 places
    blend: blend-mode,
    clip-to-below: bool,
    offset-x: s64,
    offset-y: s64,
    pixels: layer-pixels,
    meta: list<meta-entry>,     // goes to source_meta
    semantic: list<meta-entry>, // the host computes semantic_digest from it
    write: write-capability,
  }

  variant color-profile { none, embedded(bytes), named(string) }
  record canvas { width: u32, height: u32, dpi: option<f64>, profile: color-profile }

  enum compositor { generic-v1 }
  variant composite {
    render(compositor),        // the host's generic compositor (white background)
    image(bitmap),             // a composite embedded in the file (ORA, KRA mergedimage.png)
    none,
  }

  record warning { code: string, message: string }

  record structure {
    format: string,            // a name for people ("OpenRaster")
    canvas: canvas,
    layers: list<layer>,       // pre-order (parent first); siblings bottom to top (drawing order)
    composite: composite,
    file-meta: list<meta-entry>,
    context: list<u8>,         // opaque to the host; handed back to read-layer (up to 16 MiB)
    warnings: list<warning>,
  }

  variant read-error {
    not-this-format(string), corrupt(string), unsupported(string), too-large(string),
  }
  enum confidence { no, maybe, yes }

  record format-info { name: string, extensions: list<string>, mime: option<string> }
  record plugin-info {
    id: string,                // [a-z0-9-]{1,40}
    version: string,           // semver
    formats: list<format-info>,
    probe-bytes: u32,          // how much of the head probe gets (up to 64 KiB)
    can-write: bool,
  }
}

/// The source file. The plugin reads only what it needs (a 60 MB CLIP is not copied whole).
interface source {
  resource source-file {
    size: func() -> u64;
    /// Short only at EOF.
    read: func(offset: u64, len: u32) -> list<u8>;
    /// The last path element. For messages and layer names only.
    name: func() -> string;
  }
}

/// Host helpers. Each output is fixed by name (PLUGINS.md 3.3).
interface codecs {
  use types.{bytes, codec, encoded-image};
  record image-header {
    width: u32, height: u32, frames: u32, has-alpha: bool,
    dpi: option<tuple<f64, f64>>, icc: bool,
  }
  /// Run the transforms and hand the result to the plugin (small metadata such as stack.xml).
  transform: func(data: bytes, codecs: list<codec>, max-out: u32) -> result<list<u8>, string>;
  crc32: func(data: bytes) -> u32;
  sha256: func(data: bytes) -> list<u8>;
  /// Header only (no pixels are decoded).
  image-info: func(img: encoded-image) -> result<image-header, string>;
}

/// SQLite 3.50.4 (the version in rust/vendor).
interface sqlite {
  use types.{bytes};
  variant sql-value { null, integer(s64), real(f64), text(string), blob(list<u8>) }
  resource database {
    open: static func(image: bytes) -> result<database, string>;            // read only (deserialize)
    open-writable: static func(image: bytes) -> result<database, string>;   // for write-back
    query: func(sql: string, params: list<sql-value>) -> result<list<list<sql-value>>, string>;
    execute: func(sql: string, params: list<sql-value>) -> result<u64, string>;
    serialize: func() -> result<list<u8>, string>;
  }
}

/// roxmltree plus tmi-clip/src/mdp2.rs's normalisation (expat's attribute values; a DTD internal subset is refused).
interface xml {
  record element {
    name: string, namespace: option<string>,
    attributes: list<tuple<string, string>>,
    text: string, parent: option<u32>,
  }
  parse: func(text: list<u8>) -> result<list<element>, string>;
}

/// Shown by tmiki plugin test and TMI_PLUGIN_LOG only. Never affects a record.
interface log {
  enum level { debug, info }
  emit: func(level: level, message: string);
}

interface reader {
  use types.{plugin-info, confidence, structure, bitmap, read-error};
  use source.{source-file};
  info: func() -> plugin-info;
  probe: func(name: string, head: list<u8>, size: u64) -> confidence;
  read-structure: func(src: borrow<source-file>) -> result<structure, read-error>;
  /// A layer that structure marked deferred(token). Called from other instances, in parallel.
  read-layer: func(src: borrow<source-file>, context: list<u8>, token: u32) -> result<bitmap, read-error>;
  /// One chunk of codec::plugin(id). Called in parallel, tile by tile.
  decode-chunk: func(codec-id: u32, input: list<u8>, out-len: u32) -> result<list<u8>, read-error>;
}

/// Host resources for write-back.
interface edit {
  use types.{layout, codec};
  /// New pixels (held by the host).
  resource pixels {
    width: func() -> u32;
    height: func() -> u32;
    bbox: func() -> option<tuple<u32, u32, u32, u32>>;
    read: func(x: u32, y: u32, w: u32, h: u32, layout: layout) -> list<u8>;
  }
  /// Bytes the host made and holds. The plugin is told only a length and a CRC.
  resource blob {
    len: func() -> u64;
    crc32: func() -> u32;
  }
  enum zlib-kernel { zlib-v131 }               // same as tmi-clip/src/zlib.rs (CPython's zlib.compress); zlib 1.3.1
  variant encoding { raw, zlib(tuple<zlib-kernel, s8>), deflate(tuple<zlib-kernel, s8>) }
  variant image-encoding { png-v1, jpeg-keep-v1(list<u8>), webp-v1(u8) }   // jpeg-keep: the original DQT and subsampling (RUST_MIGRATION 2b-6)
  encode-tile: func(px: borrow<pixels>, x: u32, y: u32, w: u32, h: u32,
                    layout: layout, steps: list<encoding>) -> result<blob, string>;
  encode-image: func(px: borrow<pixels>, enc: image-encoding) -> result<blob, string>;
  /// The edited document drawn by the generic compositor (to rebuild an embedded composite).
  composite-after-edit: func() -> result<pixels, string>;
  thumbnail: func(px: borrow<pixels>, max-side: u32) -> pixels;
}

interface writer {
  use types.{span, blend-mode, warning};
  use source.{source-file};
  use edit.{pixels, blob};
  variant prop-change {
    name(string), visible(bool), opacity(f64), blend(blend-mode), offset(tuple<s64, s64>),
  }
  /// A borrow cannot sit inside a record, so new pixels are named by their
  /// index in the `pixels` list of write-layers. That list hands the plugin
  /// its own handles (P7: wit-bindgen 0.63's Rust guest code for a
  /// `list<borrow<pixels>>` export parameter does not compile).
  record layer-edit {
    stable-id: string,
    new-pixels: option<u32>,
    props: list<prop-change>,
  }
  /// The host joins the pieces in order into the new file (PLUGINS.md 2.2):
  /// `source` copies bytes of the file being written, `bytes` are the
  /// plugin's own, `blob` is what `edit` encoded (each blob at most once).
  variant piece { source(span), bytes(list<u8>), blob(blob) }
  record write-plan { pieces: list<piece>, warnings: list<warning> }
  variant write-error { unsupported(string), invalid(string), corrupt(string) }
  write-layers: func(src: borrow<source-file>, edits: list<layer-edit>, pixels: list<pixels>) -> result<write-plan, write-error>;
}

world format-reader {
  import source;
  import codecs;
  import sqlite;
  import xml;
  import log;
  export reader;
}

world format-plugin {
  include format-reader;
  import edit;
  export writer;
}
```

`wit/` は書き換えない。tmi はこの版の名前（`tmi:format/reader@0.1.0` など）で component をつなぐ。

### 1.2 呼び出しの約束

- **呼び出しごとに新しいインスタンス**が作られる。`info`・`probe`・`read-structure`・`read-layer`・`decode-chunk`・`write-layers` は、どれも前の呼び出しの状態を見られない。グローバル変数に何かを覚えておいて次の呼び出しで使う、という作りは動かない（`tmiki plugin test` は 1 つのインスタンスで続けて呼んでも答えが同じことも確かめる）。
- `read-structure` はファイル 1 つにつき 1 回呼ばれる。`read-layer` は deferred にしたレイヤーごとに、**別のインスタンスから並列に、どの順でも**呼ばれる。だから `read-layer` の答えは `context` と `token` とファイルの中身だけで決まらなければならない。
- `probe` は `head`（ファイルの先頭の `probe-bytes` バイト。ファイルが短ければそれだけ）と大きさだけを見る。ファイル全体は渡らない。
- 1 回の呼び出しの上限：燃料（命令数）2×10¹⁰、実時間 120 秒、メモリ 1 GiB、wasm のスタック 8 MiB（利用者が `plugin.fuel`・`plugin.timeout`・`plugin.maxMemory` で変えられる）。超えると取り込みは失敗する（「ran out of fuel」など）。`tmiki plugin test` は燃料とスタックが 1/4 でも同じ記録になることを求める。
- stdout・stderr は呼び出しごとに 1 MiB のメモリの管。記録には入らない。環境変数 `TMI_PLUGIN_LOG=1` のとき、`log.emit` の行と一緒に tmi の stderr に出る。Rust の `panic!` の文は、取り込みが失敗したときの理由として利用者に見える（`crashed (panic: ...)`）。

### 1.3 `info` と `probe`

`info`（`plugin-info`）：
- `id`：`[a-z0-9-]{1,40}`。リポジトリの `.tmiplugins/<id>.wasm` と `.tmiattributes` の `format=plugin:<id>` に使われる。
- `version`：semver 全体（`0.1.0`、`1.2.3-rc.1+build.5`）。数字 3 つのあとのプレリリース・ビルドは `[0-9A-Za-z-]` と区切りの `.` だけ（空の部分は不可）、全体で 64 文字まで。プラグインを上げるたびに上げる。記録に入る（`importer.version`）。端末にそのまま表示され、`tmiki plugin install` ではファイル名になるので、これを外れると読み込みが断られる。
- `formats`：人に見せる名前（80 文字まで・制御文字なし）、拡張子（`.kra` のように点は付けても付けなくてもよい）、MIME。拡張子は `[a-z0-9]{1,16}`（先頭の点 1 つは可）、全体で 64 個まで。`tmiki plugin add` は拡張子から `.tmiattributes` の行（`*.kra format=plugin:<id>`）を作るので、空白・改行・`*`・`=` などを含む拡張子は読み込みの時点で断られる。
- `probe-bytes`：`probe` に渡す先頭の長さ。64 KiB まで。
- `can-write`：書き戻しをするなら true（world `format-plugin` で writer を export していること。どちらかが欠ければ読むだけのプラグインとして扱う）。

`probe`：`yes`（この形式だと分かる）、`maybe`（そうかもしれない）、`no`（違う）。`.tmiattributes` で対応づけたファイルに `no` を返すと、取り込みは「the plugin looked at the start of the file and said no」で失敗する。マジックナンバーや zip の最初のメンバーの名前など、確実な印で決める。`tmiki plugin test` は、コーパスの全ファイルに `yes`、乱数のバイト列・空・全部 0・全部 0xFF に `no` を求める。

### 1.4 `structure`

- `format`：人に見せる形式名（"Krita"）。
- `canvas`：幅・高さ（ピクセル）、`dpi`（無ければ none。有限の数）、`profile`（ICC を埋め込んでいれば `embedded(bytes)`、名前だけなら `named`、無ければ `none`。記録には有無と名前だけが入る）。
- `layers`：レイヤーの木を**前順**（親が先）に平らにした列。
  - 兄弟は**下から上**（描く順。一番下の背景が先）。多くの形式のファイルは上から並べているので、逆にする。
  - `parent` は親の番号（この列の中の位置）で、**自分より前**でなければならない。根のレイヤーは none。
  - 子を持てるのは `kind: folder` のレイヤー（グループ）。
- `composite`：合成画像の選び方（1.6）。
- `file-meta`：ファイル全体のメタ情報。記録の `source_meta` に入る。
- `context`：ホストには中身が見えないバイト列。`read-layer` にそのまま渡る（16 MiB まで）。
- `warnings`：利用者に見せる警告（5 節）。

### 1.5 レイヤーの各項目

| 項目 | 意味 |
|---|---|
| `id` | 記録の `layer_id`。人や外部のツールが指すための id。ファイルが持つ id があればそれ、無ければ `stable-id` と同じでよい |
| `stable-id` | ファイルの中で一意。**同じレイヤーなら、ファイルを保存し直しても同じ値**にする（tmi はこれでレイヤーを突き合わせて diff・マージする）。ファイルがレイヤーごとの UUID や番号を持っていればそれを使う。無ければ木の中の位置（`ora:1/0` のように親からの道筋）を使う |
| `name` | レイヤー名。UTF-8 にして渡す |
| `kind` | `raster`・`folder`・`text`・`vector`・`adjustment`・`fill`・`paper`・`mask`・`stencil`・`unknown`。`mask`・`stencil`・`adjustment` は合成で描かれない |
| `visible` | 非表示なら false（合成で描かれず、非表示のフォルダーの中も描かれない） |
| `opacity` | 0〜1 の有限の数。ホストが小数 6 桁に丸めて記録する。合成では `round(opacity × 255)` |
| `blend` | 描画モード（1.1 の `blend-mode`）。形式のモードに当たるものが無ければ `other`（`normal` として描かれる）と警告。フォルダーの「通過」は `pass-through` |
| `clip-to-below` | 下のレイヤーでクリッピングする。基点は同じ親の、手前（下）にある、クリッピングでない最も近い兄弟 |
| `offset-x`・`offset-y` | レイヤーのビットマップの左上が、キャンバスのどこに来るか（ピクセル。負でもよい） |
| `pixels` | `none`（画素を持たない：フォルダー・テキストなど）、`ready(bitmap)`（recipe をここで返す）、`deferred(token)`（`read-layer` で返す。2.6） |
| `meta` | 記録の `source_meta` に入るメタ情報。キー `pixel_source`・`semantic_digest`・`semantic_digest_version`・`write_capability` と `staged_` で始まるものは使えない（ホストが書く） |
| `semantic` | 「このレイヤーの意味が変わったか」の判定に使うメタ情報。ホストがその正規 JSON の SHA-256 を `semantic_digest` として記録し、diff・マージはこれでプロパティの変化を見る。形式固有のプロパティ（ロック、色ラベルなど）を、順序の決まった形で入れる |
| `write` | このレイヤーの画素を、このプラグインの writer が書き戻せるか。`pixels`（または `pixels-and-props`）なら記録の `write_capability` が `raster-pixels`、`none` なら `metadata-only`。writer を持たないプラグインは `none` にする |

`meta-value`：`null`・`boolean`・`integer`・`number`（有限）・`text`・`sha256-of(bytes)`（ホストがその範囲の SHA-256 の 16 進を書く。大きな埋め込みデータを記録に入れずに変化だけを追う）・`json(text)`（入れ子の値。ホストが正規の JSON に直す。JSON として読めなければ取り込みは失敗）。

### 1.6 合成の選び方

- ファイルが合成画像を持っていれば（KRA・ORA の `mergedimage.png` など）`composite::image(bitmap)`。**大きさがキャンバスと同じときだけ**使われる。違えばホストは警告を出して `generic-v1` で描く（`tmiki plugin test` はこれを失敗にする）。合成画像がレイヤーの画素と同じ recipe なら、ホストはデコードを 1 回で済ませる。
- 持っていなければ `composite::render(generic-v1)`。ホストの汎用の合成器が描く。
  - 白の背景に、根のレイヤーを下から順に描く。
  - 子を持つレイヤーはグループ：`pass-through` なら子を下地に直接描き、グループの不透明度で下地に向けて薄める。それ以外は子を透明な絵に描いてから、その絵をレイヤーとして描く（分離）。子を持つレイヤー自身の画素は描かない。
  - 非表示のレイヤー、`mask`・`stencil`・`adjustment` は描かない。
  - 不透明度は alpha に `round(opacity × 255) / 255` を掛ける（切り捨て）。クリッピングは基点の alpha を掛ける。基点が描かれないとき、そのレイヤーも描かない。
  - `normal` は Pillow の `alpha_composite`、その他のモードは W3C の合成の式（`Cs' = (1 − αb)·Cs + αb·B(Cb, Cs)`）。
  - 形式のアプリの見た目と細部が違うことがある（独自のモードや、アプリ固有の合成）。違いが問題なら合成画像を持つ形式ではそれを使う。
- `composite::none`：合成画像を記録しない（勧めない。`tmiki log` と `diff` の絵が無くなる）。

### 1.7 ホストの補助関数（import）

| interface | 関数 | 約束 |
|---|---|---|
| `source` | `source-file.size()`・`read(offset, len)`・`name()` | `read` は EOF のときだけ短く返る（EOF より後は空）。`name` はファイル名の最後の要素で、メッセージとレイヤー名の既定値にだけ使う（記録をファイル名で変えない） |
| `codecs` | `transform(data, codecs, max-out)` | recipe のデコーダと同じ変換（3 節）をかけて結果をプラグインに渡す。`stack.xml`・`maindoc.xml` のような小さなメタ情報を読むため。どの段の出力も `max-out` バイトまで（超えればエラー）。`codec::plugin` は使えない |
| | `crc32(data)`・`sha256(data)` | |
| | `image-info(img)` | 画素をデコードせずにヘッダだけを読む。今は PNG と `jpeg-v1` だけ。`has-alpha` は「Pillow が開くモードに alpha の帯がある」こと |
| `sqlite` | `database.open(image)`・`open-writable(image)`・`query`・`execute`・`serialize` | SQLite 3.50.4（固定）。ファイルの中の SQLite のイメージ（span）をそのまま開く。ATTACH などファイルに届く操作は拒まれる。1 回の `query` の結果は 256 MiB まで。**順序のない SELECT の行の順に頼らない**（`ORDER BY` を付ける。`tmiki plugin test` は `PRAGMA reverse_unordered_selects=ON` でも記録が同じことを確かめる） |
| `xml` | `parse(text)` | 要素を文書順に平らにした列（`parent` は親の番号）。属性名は名前空間付きなら `{uri}name`。`text` は ElementTree の `.text`（最初の子要素より前のテキストと CDATA）。DTD の内部サブセットは断る。XML は component の中で読む（roxmltree）ほうを勧める。ホストの `xml` は MDP2 の読み方に固定した便利な関数 |
| `log` | `emit(level, message)` | `TMI_PLUGIN_LOG=1` のときだけ表示。記録に影響しない |
| `edit` | 6 節 | 書き戻しの呼び出しの中でだけ使える |

WASI：component が import してよいのは `tmi:format/*` と WASI 0.2 の cli・io・clocks・filesystem（types・preopens）・random だけ。`wasi:sockets`・`wasi:http` などを import する component は、tmi が読み込みの時点で断る。ファイルシステムは空、環境変数・引数は空、stdin は閉じている、時計は 0 で止まっている、乱数は固定の種。Rust の `wasm32-wasip2` の std は上の範囲に収まる。

大きさ：1 つの component が持てる core instance（中で instantiate される core module）は 16 個、メモリは 4 個、テーブルは 8 個まで。`tmi-plugin-sdk` の上の Rust の component は core instance 4〜5 個・メモリ 1 個・テーブル 2 個で、十分に収まる。超える component は読み込みの時点で断られる。

### 1.8 ホストの検査（違反すれば取り込みは失敗し、何も記録されない）

- レイヤーは 10 万個まで。`stable-id` は一意。`parent` は自分より前の番号。
- `opacity` は 0〜1 の有限の数。`number` のメタ情報と `dpi` は有限。予約したメタ情報のキーは使えない。
- すべての `span` がファイルの中に収まる。`inline` は 1 つ 64 MiB まで、1 つの structure で合計 256 MiB まで（`raw-pixels` の画素も数える）。`context` は 16 MiB まで。
- ビットマップ・タイル・キャンバスの画素数は利用者の上限（`core.maxImagePixels`）の範囲。展開は 1 回 1 GiB まで。
- すべての recipe がデコードできる（画像の形式が `codec` の名前と合う、タイルの画像の大きさがタイルと同じ、など。3 節）。

## 2. recipe：画素の描き方

### 2.1 ビットマップと座標

レイヤーの画素は `bitmap` で、RGBA8（ストレートアルファ）の幅×高さの絵になる。その絵の左上がキャンバスの `(offset-x, offset-y)` に置かれる。

- `pixel-source::image(placed-image)`：画像 1 枚を、ビットマップの `(x, y)` に置く。`bitmap.size` が none なら、ビットマップの大きさは画像の大きさ（このときふつう `x = y = 0`）。
- `pixel-source::tiles(tiled)`：タイルを並べる。`bitmap.size` は必須。タイルの `(x, y)` はビットマップの中の位置で、はみ出した分は切られる。
- `pixel-source::raw(raw-pixels)`：プラグインが計算した RGBA8 の行（最後の手段。インスタンスの間で大きなデータをコピーすることになる）。
- `pixel-source::empty`：透明（`size` があればその大きさ、無ければ 0×0）。

レイヤーのビットマップをキャンバスの大きさにする必要はない。形式がレイヤーごとの外接矩形を持っていれば、その大きさのビットマップと offset で返す。

### 2.2 `bytes` と `codec`

`bytes` はホストがファイルから取り出すバイト列：`span(offset, len)`（ファイルの範囲）、`spans([...])`（範囲をつなげたもの。分割されたストリームや、チャンネルごとの範囲）、`inline(list<u8>)`（プラグインが作ったもの。小さいものだけ）。大きなデータを `inline` で返さない。プラグインを通るだけで遅くなる。

`codec` はバイト列の変換で、列の順にかける。`raster` の `codecs` の最後の段の出力は、レイアウトが決める長さ（2.4）でなければならない。定義は 3 節。

### 2.3 画像（`encoded-image`）

ホストが自分でデコードする画像：`png`・`jpeg-v1`・`gif`・`bmp`・`tiff`・`webp-v1`（`qoi` はまだデコーダが無い）。`pre` は画像の前に剥がす変換（zip の deflate のメンバーなら `[deflate]`）。`frame` は 0 だけ（複数フレームの 2 枚目以降はまだ読めない）。中身の形式が `codec` と違えば失敗。RGBA への変換は Pillow 12.3 と同じ（パレット・灰色・16 ビットの PNG も）。

### 2.4 タイルと `layout`

`tile-content::raster(raster)` は、`codecs` で展開したバイト列を `layout` で RGBA8 にする：

- 展開したバイト列は、`groups` を順につなげたもの。各群はタイルの**全画素**を持つ（群ごとに平面）。群の中は行が上から下、各行は `ceil(幅 × チャンネル数 × ビット数 / 8)` バイトの画素（チャンネルは群の中で画素ごとに交互）と、その後ろの `row-padding` バイト。
- 長さが `Σ 群 (行のバイト数 + row-padding) × 高さ` でなければ失敗。
- 8 ビットへの変換：`one-bit-msb` は行の上位ビットから 0→0・1→255、`eight` はそのまま、`sixteen-be`・`sixteen-le` は上位バイト（`v >> 8`）、`float32-le` は 0〜1 に切って ×255 を f64 で四捨五入（NaN は 0）。
- 画素は `[0, 0, 0, 255]` から始まり、群の順に各チャンネルを書く（後の群が上書き）。`gray` は r・g・b に書き、`pad` は捨てる。どの群にも `a` が無ければ alpha は 255。
- `premultiplied` なら Pillow の RGBa→RGBA（`c × 255 / a` の整数版）で戻す。そのあとで `tint`：色のチャンネル（r・g・b・gray）がどこにも無ければ rgb を `tint.rgb` にし、`tint` があれば alpha を `a × tint.a / 255`（切り捨て）にする。
- `tile-content::image`：画像の大きさはタイルと同じでなければならない。`fill(rgba8)`：一色。`empty`：何も置かない。
- `overlap`：`replace`（後のタイルが上書き）、`over`（後のタイルを `alpha_composite` で重ねる）、`reject`（ビットマップで切ったあとの画素が重なれば失敗）。

### 2.5 例

**zip の中の PNG（ORA・KRA の合成画像）**。stored のメンバーはそのまま、deflate のメンバーは `pre: [deflate]`。メンバーの中身の位置は、ローカルヘッダの `30 + 名前の長さ + extra の長さ` の後ろ。

```
bitmap { size: none, source: image({ x: 0, y: 0, image: {
    data: span(12_345, 80_000), pre: [deflate], codec: png, frame: 0 } }) }
```

**CLIP 式の平面の群**（256×256 のタイル、zlib。alpha の平面のあとに BGR＋詰め物の平面）：

```
tiles { overlap: replace, tiles: [ { x: 0, y: 0, width: 256, height: 256, content: raster({
    data: span(off, len), codecs: [zlib],
    layout: { groups: [ { channels: [a], depth: eight },
                        { channels: [b, g, r, pad], depth: eight } ],
              row-padding: 0, tint: none, premultiplied: false } }) }, ... ] }
```

**64 px の平面のタイル、LZF**（Krita の 1 タイル：64×64、B・G・R・A の 4 平面、LZF で圧縮。タイルの頭に圧縮の印の 1 バイトがあるなら、span はその後ろから）：

```
{ x: 64 * col, y: 64 * row, width: 64, height: 64, content: raster({
    data: span(off + 1, len - 1), codecs: [lzf(64 * 64 * 4)],
    layout: { groups: [ { channels: [b], depth: eight }, { channels: [g], depth: eight },
                        { channels: [r], depth: eight }, { channels: [a], depth: eight } ],
              row-padding: 0, tint: none, premultiplied: false } }) }
```

`bitmap.size` はタイルの範囲を覆う大きさ（`(最大の col + 1) × 64` など）にし、レイヤーの原点に合わせて `offset-x`・`offset-y` を決める。タイルの座標が負になりうる形式では、最小の座標を offset に移してタイルの `x`・`y` を 0 以上にする。

**MDP2 式の疎なタイル＋`over`**（中身のあるタイルだけを並べ、重なる所はアルファで重ねる）：`overlap: over` にして、空のタイルは並べない。

**PSD 式のチャンネル別 packbits**（R・G・B・A が別々の RLE のストリーム。行ごとのバイト数の表は飛ばす）：

```
raster({ data: spans([R の圧縮データ, G の, B の, A の]), codecs: [packbits(w * h * 4)],
         layout: { groups: [ {channels: [r], depth: eight}, {channels: [g], depth: eight},
                             {channels: [b], depth: eight}, {channels: [a], depth: eight} ],
                   row-padding: 0, tint: none, premultiplied: false } })
```

PackBits は終わりの印を持たないので、別々に圧縮したストリームをつなげて展開すると、展開したものをつなげたものになる。チャンネルの数だけの範囲を `spans` に並べる。

**1 ビット＋tint**（線画のマスク：1 ビットの alpha を、黒の色で描く）：

```
layout: { groups: [ { channels: [a], depth: one-bit-msb } ], row-padding: 0,
          tint: some({ r: 0, g: 0, b: 0, a: 255 }), premultiplied: false }
```

1 ビットの行は、行ごとにバイトの境目に切り上がる（幅 10 なら 2 バイト）。

**16 ビット**：`depth: sixteen-be`（または `sixteen-le`）。ホストは上位バイトを使う（`v >> 8`）。8 ビットへの丸め方が形式のアプリと違うときは、警告を出すか、`raw` で自分で丸めた値を返す。

**PNG の行フィルタが外にある形式**：`codecs: [zlib, predictor({ kind: png-rows, row-bytes: w * 4, bytes-per-pixel: 4 })]`。TIFF の水平差分は `tiff-horizontal`（1 バイト単位の差分）。

### 2.6 `deferred` と `context`

recipe を作るのに時間がかかるレイヤー（タイルの表を全部たどる、行の表を走査するなど）は `deferred(token)` にする。ホストは `read-layer(src, context, token)` を、別のインスタンスから並列に呼ぶ。答えは `context`（`structure.context` の中身）・`token`・ファイルだけで決める。レイヤーの数が多く、1 つずつの recipe が重いときに使う。安く作れるなら `ready` でまとめて返す（呼び出しが 1 回で済み、速い）。どちらにしても記録は同じでなければならない（`tmiki plugin test` が逆順に読んでも同じことを確かめる）。

### 2.7 `codec::plugin` と `decode-chunk`

ホストが持たない圧縮は、`codec::plugin(id)` にすると、ホストがチャンクごとに `decode-chunk(id, input, out-len)` を並列に呼ぶ（`out-len` は最後の段のときだけ期待する長さ、それ以外は 0）。返す長さが期待より長ければ失敗。wasm の中のデコードはネイティブより遅いので、3 節のホストの codec で表せるならそちらを使う。`codec::plugin` を使う recipe の記録の `pixel_source` には component の SHA-256 が入る（プラグインを上げると、画素が同じでもデコードし直しになる）。

## 3. codec の厳密な定義

| codec | 入力 | 失敗する場合 |
|---|---|---|
| `zlib` | RFC 1950 のストリーム（adler32 を確かめる） | ストリームが終わらない、adler32 が違う、出力が上限を超える。**終わりより後ろのバイトは無視** |
| `deflate` | RFC 1951 の生の deflate | 同上（終わりの後ろは無視） |
| `gzip` | RFC 1952（1 メンバー） | 壊れている、上限を超える |
| `zstd` | zstd のフレーム | 同上 |
| `lz4-block(n)` | LZ4 のブロック形式（フレームではない） | 展開がちょうど `n` バイトでない、参照が範囲の外 |
| `lzf(n)` | liblzf の形式（Krita のタイル） | 展開がちょうど `n` バイトでない、参照が範囲の外 |
| `packbits(n)` | PackBits（PSD・TIFF の RLE。-128 は何もしない） | 展開がちょうど `n` バイトでない |
| `predictor(png-rows, row-bytes, bpp)` | 「フィルタ型 1 バイト＋行」の並び（PNG の 5 種のフィルタ） | 入力が `row-bytes + 1` の倍数でない、フィルタ型が 0〜4 でない |
| `predictor(tiff-horizontal, row-bytes, bpp)` | 行ごとに `bpp` バイト前との差分（1 バイト単位、mod 256） | 入力が `row-bytes` の倍数でない |
| `plugin(id)` | 2.7 | プラグインの `read-error`、長すぎる出力 |

- 列の最後の段の出力の長さは、`raster` ならレイアウトの長さとちょうど同じでなければならない（短くても長くても失敗）。画像（`encoded-image.pre`）とメタ情報（`codecs.transform`）では長さの決まりは無い（上限だけ）。
- どの段も、展開は 1 GiB まで。
- 実装で結果が変わりうるもの（JPEG のデコード、非可逆の WebP）は名前に世代が付く（`jpeg-v1`）。tmi は一度出した世代の出力を変えない。

## 4. 決定性

記録は**元ファイルのバイト・ファイル名・component のバイトだけで決まる**。OS・CPU・tmi の版・呼び出しの順に依らない。プラグインが守ること：

- 出力（structure・recipe・警告・write-plan）を、ファイルの中身だけから計算する。ファイル名は `name` のレイヤー名の既定値くらいにしか使わない。
- 呼び出しの間で状態を持たない（1.2）。キャッシュを作らない。
- 浮動小数を id や順序の決め手にしない。`opacity` は形式が持つ値から毎回同じ式で計算する。
- 順序の決まらないものに頼らない：
  - `HashMap`・`HashSet` の反復順（`BTreeMap` か、並べてから使う）。
  - SQLite の `ORDER BY` の無い SELECT の行の順。
  - 時刻・乱数・環境変数（サンドボックスでは固定の値が返るので、使うと全員が同じ値を記録する。意味が無い）。
  - `f64` の `Display` の文字列を id にしない。

`tmiki plugin test` の「2 determinism」がこれらを確かめる：新しいインスタンスで 2 回、deferred を逆順に 1 つずつ、1 つのインスタンスで全部の呼び出し、SQLite の `reverse_unordered_selects`、スタック 1/4、燃料 1/4。

## 5. エラーと警告

`read-error`：

| case | 使うとき | ホストの扱い |
|---|---|---|
| `not-this-format(msg)` | この形式のファイルではない（マジックが違う、必須のメンバーが無い） | 利用者には「it does not look like a file this plugin reads」 |
| `corrupt(msg)` | この形式だが壊れている（途中で切れている、表が範囲の外、XML が読めない） | 「the file is damaged」 |
| `unsupported(msg)` | 壊れてはいないが、このプラグインが読まない機能（zip64、未知の圧縮、新しい版） | 「it uses something the plugin does not support」 |
| `too-large(msg)` | 大きすぎて読まない（自分で決めた上限を超える） | 「it is too large」 |

- メッセージは利用者が読む英語の 1 文。何が・どこで（メンバー名・レイヤー番号・オフセット）を入れる。
- 既定（`.tmiattributes` の `on-error=refuse`）では、読めないファイルは記録されず、`tmiki add` がそのファイルを一覧にして断る。
- **panic・例外で落とさない**。落ちると「crashed」になり、利用者には理由が伝わらない。`tmiki plugin test --fuzz` は落ちた件を警告として挙げる。入力の長さ・オフセット・個数は必ず確かめてから使う（`source-file.read` は EOF で短く返るだけで失敗しない）。

`warning { code, message }`：読めたが、記録が元のアプリの見た目と違いうるとき（未知の描画モードを `other` にした、読まない要素を飛ばした）。`code` は `[a-z][a-z0-9-]*`、40 文字まで（`blend`・`element` のような短い分類）。ホストは `tmiki: warning: <file>: <message> (<code>)` と表示する。記録には入らない。

## 6. 書き戻し（world `format-plugin`）

`write-layers(src, edits, pixels)`：

- `edits`：書き換えるレイヤー（`stable-id`）と、新しい画素の番号（`new-pixels`。`pixels` の列の位置）。`props`（名前・表示・不透明度・モード・オフセット）は今の tmi はまだ渡さない。対応しないなら `unsupported` を返す。
- `pixels`：新しい画素（`edit.pixels`）。レイヤーのビットマップ全体で、左上がレイヤーの `offset-x`・`offset-y` に来る。
- 返すのは `write-plan { pieces, warnings }`。ホストが `pieces` を順につなげて新しいファイルにする：
  - `source(span)`：元のファイルの範囲をそのまま写す（大きなバイト列をプラグインに通さない）。
  - `bytes(list<u8>)`：プラグインが作ったバイト（ヘッダ・目次など。合計 256 MiB まで）。
  - `blob(blob)`：ホストが符号化したバイト列（`edit.encode-image`・`encode-tile` の結果）。各 blob は 1 回だけ置ける。プラグインは長さ（`len`）と CRC-32（`crc32`）だけを知り、それで zip の目次やオフセットの表を組み立てる。

`edit` の関数：
- `pixels.width/height/bbox`（alpha が 0 でない画素の外接矩形 `(x0, y0, x1, y1)`。全部透明なら none）。
- `pixels.read(x, y, w, h, layout)`：範囲を `layout` の形のバイト列にする（`eight`・`sixteen-*` だけ。16 ビットは 8 ビットの値 ×257、`gray` は Pillow の L、`premultiplied` は Pillow の RGBA→RGBa。`tint` は断る）。
- `encode-tile(px, x, y, w, h, layout, steps)`：`read` してから `raw`・`zlib(zlib-v131, level)`・`deflate(zlib-v131, level)` を順にかけた blob。zlib 1.3.1 で圧縮するので、同じ画素からは同じバイトになる。
- `encode-image(px, png-v1)`：tmi の PNG の符号化器（8 ビット RGBA）。`jpeg-keep-v1`・`webp-v1` はまだ無い（エラーを返す）。
- `composite-after-edit()`：編集後の文書を `generic-v1` で**透明の背景に**描いた絵（キャンバスの大きさ）。ファイルが埋め込みの合成画像を持つとき、それを作り直すのに使う。
- `thumbnail(px, max-side)`：縦横比を保って `max-side` に収めた縮小（Pillow の `thumbnail`、LANCZOS）。

守ること：
- 編集しないレイヤーとメタ情報のバイトは元のまま写す。
- **冪等**：書いたファイルに同じ画素をもう一度書くと、同じバイトになる（`tmiki plugin test` が確かめる）。できれば、元の画素をそのまま書いたときも元のバイトになる（符号化器が違えば変わってよい。警告にとどまる）。
- 合成画像・サムネイルを持つ形式は、`composite-after-edit` と `thumbnail` から作り直す（古い合成を残すと、アプリが古い絵を見せる）。
- ホストは、つなげた結果を同じプラグインで読み直して次を確かめ、通らなければ何も書かない：キャンバスが同じ、レイヤーの木（数・順・親）が同じ、全レイヤーのプロパティと `source_meta` が同じ、編集したレイヤーの画素が新しい画素と同じ、ほかのレイヤーの画素が元のまま。読み直したときに編集したレイヤーの recipe が新しい画素を描くよう、目次（オフセット・長さ・CRC）を正しく直す。

`write-error`：`unsupported`（その編集はできない）、`invalid`（ホストの渡したものがおかしい）、`corrupt`（元のファイルが壊れていて書けない）。

## 7. コーパスの作り方（`tests/corpus/`）

- 形式のアプリで作ったファイルと、手で（スクリプトで）作ったファイルの両方を置く。小さいものから（数十〜数百 KB）。
- 1 つの機能に 1 ファイル：レイヤー 1 枚、全レイヤー型、全描画モード（1 ファイルに並べてよい）、入れ子のフォルダー、通過のフォルダー、非表示、不透明度、クリッピング、負のオフセット、空のレイヤー、日本語のレイヤー名、合成画像あり・なし、圧縮の有無、16 ビット・1 ビットなど形式が持つ深さ。
- 壊れたファイルはコーパスに入れない（全部 `yes` で読めることを求めるため）。壊れた場合は `--fuzz` が作る。
- 記録が正しいことを確かめたら `tmiki plugin test --update` で `<file>.expect.json` を書く。以後の変更で記録が変わると「7」が失敗する（意図した変更なら `--update` し直す）。
- サンプルの出どころ・作り方は SPEC.md の「サンプル」の表に書く。

## 8. 完成の条件

- `tmiki plugin build` が通る（component の検査・import の許可表・`info` の検査を含む）。
- `tmiki plugin test` が 0 で終わる。各項目：
  - `1 native = wasm`：同じコードの開発用プラグインと比べる（`--native`。11 節）。
  - `2 determinism`：4 節。
  - `3`：ホストの検査と全 recipe のデコード（1.8）、`stable-id` の一意、警告のコード、合成画像の大きさ、`probe`。
  - `4 round trip`（writer があれば）：各レイヤーを元の画素で書いて読み直す、2 回書いて同じバイト、試験の模様（端の 1 画素・全面透明・α 0 の下の色）で書いてホストの照合を通る。
  - `7`：`*.expect.json` と同じ記録。
- `tmiki plugin test --fuzz 10000` で `host panics` が 0（`traps` も 0 が望ましい）。
- SPEC.md の「Open items」に、分かっていないこと・読まない機能・アプリと見た目が違いうる所が書いてある。
- 版を上げたら `tmiki plugin test --against <古い component>` で記録の違いを確かめる（`pixels changed` の行は、利用者のファイルが「変わった」と見えるレイヤー）。

## 9. AI に渡す指示文の雛形

```
あなたは tmi の形式プラグイン <id>（<形式名>、拡張子 <.ext>）を Rust で書く。
作業ディレクトリは tmiki plugin new <id> [--writer] で作ったもの。
1. PLUGIN_SPEC.md を全部読む。AGENTS.md の手順に従う。
2. 10 節の完全な例（OpenRaster）を読む。tests/corpus/ のサンプルを調べ、分かったこと（オフセット・大きさ・意味・どのサンプルで確かめたか）を SPEC.md に書く。
3. src/lib.rs の probe と read-structure を書く（画素はデコードせず、span と codec で recipe を返す）。
4. tmiki plugin build、tmiki plugin test --native を回し、FAIL の行を全部直す。0 で終わるまで繰り返す。調べるときは開発用プラグイン（11 節）で eprintln! を使う。
5. 記録を確かめたら tmiki plugin test --update、最後に tmiki plugin test --fuzz 10000。
6. 分からないこと・読まない機能は推測で埋めず、SPEC.md の Open items に書く。
形式の資料：<URL や仕様書のパス>
```

## 10. 完全な例：OpenRaster（Rust、`tmi-plugin-sdk`）

OpenRaster（`.ora`）は zip で、`stack.xml`（レイヤーの木。**上から**並ぶ。`<stack>` が入れ子のグループ、`<layer src x y opacity visibility name composite-op>` がラスター）、レイヤーごとの PNG、合成画像 `mergedimage.png`、サムネイル `Thumbnails/thumbnail.png` を持つ。下は reader と writer の全部で、tmi のリポジトリの `rust/plugins/ora` そのもの（tmi のテストがこの節と同じであることを確かめている）。tmi が試験と計測に使うプラグインで、同じコードを 3 通りに作る：`wasm32-wasip2` の component（`export_plugin!`）、feature `dev` の開発用プラグイン `ora-dev`（11 節）、tmi のホストのテストが同じプロセスの中で component と比べるネイティブのライブラリ。

要点：
- コア（`lib.rs`・`zip.rs`・`write.rs`）は SDK の型を直接使わず、`model.rs` の素朴な型と trait `Host`・`WriteHost` を通してファイルとホストを見る。`plugin.rs` がそれを SDK の型に一対一で写す。tmi がネイティブでも同じコアを試すための分け方で、自分のプラグインでは雛形のように SDK の型を直接使えばよい。
- zip は中央ディレクトリから自分で読む（`zip.rs`）。名前は UTF-8 として読み（読めない所は置き換え）、同じ名前の後のメンバーが前を上書きし、stored・deflate 以外のメンバーは表から外す。メンバーの中身はコピーせず、ローカルヘッダの後ろの span を返す。プラグインを通るのは目次と `stack.xml` だけ（`codecs::transform` で展開）。
- `stack.xml` は component の中で roxmltree で読む（1.7 の `xml` の行の勧めどおり。ホストの `xml` は使わない）。
- 各レイヤーは `ready` で、PNG の span を返す（deflate のメンバーなら `pre: [deflate]`）。`<image tmi-test-defer="1">` のファイルだけは `deferred`（tmi の試験用の属性。`read-layer` の道を同じファイルで通すため）。
- `stable-id` は木の中の道筋（`ora:1/0`：根の 2 番目のグループの 1 番目。下から数える）。ORA はレイヤーの id を持たないため。
- 未知の `composite-op` は `other` と警告 `blend`。
- 読めないときは `model::Error` を返し、`plugin.rs` が `ReadError` に直す。panic しない（長さ・オフセットは確かめてから使う）。
- 書き戻しは zip を作り直す：触らないメンバーは `source`、編集したメンバーは新しいローカルヘッダ（`bytes`）＋ホストの PNG（`blob`）、合成画像とサムネイルは `composite-after-edit` と `thumbnail` から、中央ディレクトリはオフセットと CRC を直して `bytes`。`<image tmi-test-write-fault=...>` は、ホストの照合が間違った writer を止めることを試すための、わざと間違える試験用の属性。
- `info` の版と `can-write` は、build のときの環境変数 `TMI_ORA_VERSION`・`TMI_ORA_READ_ONLY` で変えられる（tmi の conformance が複数の版を作るため）。自分のプラグインでは定数で書けばよい。

このコードを `tmiki plugin new` のディレクトリに置いたときの `tmi-plugin.toml`（開発用プラグインはこの crate の `ora-dev`）：

```toml
[plugin]
id = "ora"
lang = "rust"
world = "format-plugin"

[build]
output = "target/tmi-plugin/ora.wasm"

[test]
corpus = "tests/corpus"

[dev]
command = "target/debug/ora-dev"
```

### `Cargo.toml`

パッケージ。`[lib]` が cdylib（component）と rlib（tmi のテストがネイティブで使う）、feature `dev` が開発用プラグイン。`path = "../sdk"` は tmi のリポジトリの中の置き場所で、`tmiki plugin new` のディレクトリでは `sdk/tmi-plugin-sdk` になる。

```toml
[package]
name = "tmi-plugin-ora"
description = "OpenRaster reader and writer for tmi (PLUGINS.md 7.3): written on tmi-plugin-sdk, a wasm32-wasip2 component and (feature dev) the same code as a development plugin; the reader's core is also a plain native library"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib", "rlib"]

[features]
# the native development plugin (src/bin/ora-dev.rs, tmiki plugin dev)
dev = ["dep:tmi-plugin-sdk"]

[[bin]]
name = "ora-dev"
required-features = ["dev"]

[dependencies]
roxmltree = "0.20"
# natively only with the feature: tmi-plugin-host's native ORA (OraNative)
# uses the reader's core without the SDK
tmi-plugin-sdk = { path = "../sdk", optional = true }

[target.'cfg(target_arch = "wasm32")'.dependencies]
tmi-plugin-sdk = { path = "../sdk" }
```

### `src/lib.rs`

reader の本体：`info`・`probe`・`read_structure`（`stack.xml` を roxmltree で読み、木を下から上へたどる）・`read_layer`。

```rust
//! An OpenRaster (`.ora`) reader and writer for tmi (PLUGINS.md 7.3),
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
```

### `src/model.rs`

コアの素朴な型と、ファイルとホストの関数を見る trait `Host`・`WriteHost`。

```rust
//! What the reader returns, before either side converts it: the subset of
//! `tmi:format/types` an OpenRaster file uses.

/// A part of the source file (absolute offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

/// A zip member's stored bytes: deflated or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub span: Span,
    pub deflate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raster,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    Normal,
    PassThrough,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    LinearDodge,
    Hue,
    Saturation,
    Color,
    Luminosity,
    Other,
}

/// A layer's pixels: a PNG member placed at (0, 0) of its own bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pixels {
    None,
    Ready(Member),
    Deferred(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub id: String,
    pub stable_id: String,
    pub parent: Option<u32>,
    pub name: String,
    pub kind: Kind,
    pub visible: bool,
    pub opacity: f64,
    pub blend: Blend,
    pub offset_x: i64,
    pub offset_y: i64,
    pub pixels: Pixels,
    /// Text entries of `meta`.
    pub meta: Vec<(String, String)>,
    /// Text entries of `semantic`.
    pub semantic: Vec<(String, String)>,
    /// `write-capability::pixels` (else `none`).
    pub writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composite {
    /// `mergedimage.png`.
    Image(Member),
    /// `render(generic-v1)`.
    Render,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Structure {
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub layers: Vec<Layer>,
    pub composite: Composite,
    pub context: Vec<u8>,
    /// (code, message).
    pub warnings: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NotThisFormat(String),
    Corrupt(String),
    Unsupported(String),
    TooLarge(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    No,
    Maybe,
    Yes,
}

pub struct Info {
    pub id: &'static str,
    pub version: &'static str,
    pub format_name: &'static str,
    pub extensions: &'static [&'static str],
    pub mime: Option<&'static str>,
    pub probe_bytes: u32,
    /// `plugin-info.can-write`.
    pub can_write: bool,
}

// -- the writer --------------------------------------------------------------

/// A layer to write: its stable id and the host's handle of its new pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub stable_id: String,
    pub pixels: u32,
}

/// `writer.piece`: a blob is the host's handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Source(Span),
    Bytes(Vec<u8>),
    Blob(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub pieces: Vec<Piece>,
    /// (code, message).
    pub warnings: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteFail {
    Unsupported(String),
    Invalid(String),
    Corrupt(String),
}

/// The host's `edit` helpers, as the writer sees them (handles are numbers).
pub trait WriteHost: Host {
    /// `edit.encode-image(px, png-v1)`: a blob handle.
    fn encode_png(&mut self, pixels: u32) -> Result<u32, String>;
    /// `blob.len` and `blob.crc32`.
    fn blob_info(&self, blob: u32) -> (u64, u32);
    /// `edit.composite-after-edit`: a pixels handle.
    fn composite_after_edit(&mut self) -> Result<u32, String>;
    /// `edit.thumbnail(px, max-side)`: a pixels handle.
    fn thumbnail(&mut self, pixels: u32, max_side: u32) -> u32;
}

/// The file and the host's helper, as the plugin sees them.
pub trait Host {
    /// `source-file.size`.
    fn size(&self) -> u64;
    /// `source-file.read`: short only at the end of the file.
    fn read(&self, offset: u64, len: u32) -> Vec<u8>;
    /// `codecs.transform(span, [deflate], max)`.
    fn inflate(&self, member: &Member, max_out: u32) -> Result<Vec<u8>, String>;
}
```

### `src/zip.rs`

zip の中央ディレクトリを `source-file.read` で読む。メンバーの中身は span で返す。

```rust
//! The zip directory, read through `Host::read` (the members' bytes are
//! only pointed at, never copied into the plugin), and what the writer needs
//! to rebuild it ([`Directory`]: every central directory record and where
//! each member's bytes run).

use std::collections::HashMap;

use crate::model::{Error, Host, Member, Span};

pub struct Archive {
    /// Each name's last member (as zipfile's `getinfo`).
    entries: HashMap<String, Member>,
    /// The local header offset of each name's last member.
    locals: HashMap<String, u64>,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn corrupt(m: impl Into<String>) -> Error {
    Error::Corrupt(m.into())
}

/// One central directory record.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    /// Where the record sits in the file, and its length.
    pub record: Span,
    pub method: u16,
    /// The stored (compressed) size.
    pub csize: u64,
    pub local: u64,
}

/// The end of central directory record and the directory before it.
#[derive(Clone, Debug)]
pub struct Directory {
    pub entries: Vec<Entry>,
    pub cd_offset: u64,
    pub cd_size: u64,
    /// The end of central directory record (with its comment), to the end of the file.
    pub eocd: Span,
}

impl Directory {
    pub fn open(host: &dyn Host) -> Result<Directory, Error> {
        let size = host.size();
        if size < 22 {
            return Err(corrupt("it is empty or cut short, too short for a ZIP archive"));
        }
        // the end of central directory record: within the last 64 KiB + 22 bytes
        let tail_len = size.min(22 + 0xFFFF);
        let tail_at = size - tail_len;
        let tail = host.read(tail_at, tail_len as u32);
        let eocd = (0..=tail.len() - 22)
            .rev()
            .find(|&i| tail[i..i + 4] == *b"PK\x05\x06")
            .ok_or_else(|| corrupt("the archive is cut short, or it is not a ZIP file: it has no end of central directory record"))?;
        let e = &tail[eocd..];
        let count = u16le(e, 10) as usize;
        let cd_size = u32le(e, 12) as u64;
        let cd_off = u32le(e, 16) as u64;
        if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            return Err(Error::Unsupported("zip64 archives are not read".into()));
        }
        if cd_off.checked_add(cd_size).is_none_or(|end| end > size) || cd_size > (64 << 20) {
            return Err(corrupt("the central directory is outside the file"));
        }
        let cd = host.read(cd_off, cd_size as u32);
        let mut entries = Vec::with_capacity(count);
        let mut at = 0usize;
        for _ in 0..count {
            if at + 46 > cd.len() || cd[at..at + 4] != *b"PK\x01\x02" {
                return Err(corrupt("a broken central directory entry"));
            }
            let method = u16le(&cd, at + 10);
            let csize = u32le(&cd, at + 20) as u64;
            let n = u16le(&cd, at + 28) as usize;
            let m = u16le(&cd, at + 30) as usize;
            let k = u16le(&cd, at + 32) as usize;
            let local = u32le(&cd, at + 42) as u64;
            let name_bytes = cd.get(at + 46..at + 46 + n).ok_or_else(|| corrupt("a name runs past the directory"))?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();
            // (a record running past the directory is the writer's to refuse:
            // the reader never needed its tail)
            let len = 46 + n + m + k;
            if csize == 0xFFFF_FFFF || local == 0xFFFF_FFFF {
                return Err(Error::Unsupported("zip64 members are not read".into()));
            }
            entries.push(Entry { name, record: Span { offset: cd_off + at as u64, len: len as u64 }, method, csize, local });
            at += len;
        }
        Ok(Directory { entries, cd_offset: cd_off, cd_size, eocd: Span { offset: tail_at + eocd as u64, len: (tail.len() - eocd) as u64 } })
    }
}

impl Archive {
    pub fn open(host: &dyn Host) -> Result<Archive, Error> {
        let dir = Directory::open(host)?;
        let size = host.size();
        let mut entries = HashMap::with_capacity(dir.entries.len());
        let mut locals = HashMap::with_capacity(dir.entries.len());
        for e in &dir.entries {
            let csize = e.csize;
            let deflate = match e.method {
                0 => false,
                8 => true,
                // kept out of the table: naming it later is an error
                _ => continue,
            };
            let head = host.read(e.local, 30);
            if head.len() < 30 || head[..4] != *b"PK\x03\x04" {
                return Err(corrupt(format!("{}: no local header", e.name)));
            }
            let start = e.local + 30 + u16le(&head, 26) as u64 + u16le(&head, 28) as u64;
            if start.checked_add(csize).is_none_or(|end| end > size) {
                return Err(corrupt(format!("{}: its data runs past the file", e.name)));
            }
            // a later member of the same name replaces an earlier one
            entries.insert(e.name.clone(), Member { span: Span { offset: start, len: csize }, deflate });
            locals.insert(e.name.clone(), e.local);
        }
        Ok(Archive { entries, locals })
    }

    /// The last member named `name` (as zipfile's `getinfo`).
    pub fn member(&self, name: &str) -> Option<Member> {
        self.entries.get(name).copied()
    }

    /// The local header offset of the last member named `name`.
    pub fn local(&self, name: &str) -> Option<u64> {
        self.locals.get(name).copied()
    }
}
```

### `src/plugin.rs`

`tmi_plugin_sdk::Plugin` の実装：コアの型を SDK（WIT）の型に一対一で写す。`dev_fault` は tmi の `tmiki plugin dev` の試験用。

```rust
//! The plugin itself: [`Ora`] implements `tmi_plugin_sdk::Plugin` over the
//! reader and writer of this crate, the same code for the component
//! (`export_plugin!` in lib.rs, `wasm32-wasip2`) and for the development
//! plugin (`src/bin/ora-dev.rs`, `tmiki plugin dev`).

use crate::model as m;
use tmi_plugin_sdk::{self as sdk, codecs, edit, Plugin, SourceFile};

pub struct Ora;

struct SdkHost<'a, 'b> {
    src: &'a SourceFile<'b>,
}

impl m::Host for SdkHost<'_, '_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len)
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        codecs::transform(&span_bytes(member.span), &[sdk::Codec::Deflate], max_out)
    }
}

fn span_bytes(s: m::Span) -> sdk::Bytes {
    sdk::Bytes::Span(sdk::Span { offset: s.offset, len: s.len })
}

fn error(e: m::Error) -> sdk::ReadError {
    match e {
        m::Error::NotThisFormat(s) => sdk::ReadError::NotThisFormat(s),
        m::Error::Corrupt(s) => sdk::ReadError::Corrupt(s),
        m::Error::Unsupported(s) => sdk::ReadError::Unsupported(s),
        m::Error::TooLarge(s) => sdk::ReadError::TooLarge(s),
    }
}

fn png(member: m::Member) -> sdk::Bitmap {
    let image = sdk::EncodedImage {
        data: span_bytes(member.span),
        pre: if member.deflate { vec![sdk::Codec::Deflate] } else { vec![] },
        codec: sdk::ImageCodec::Png,
        frame: 0,
    };
    sdk::Bitmap { size: None, source: sdk::PixelSource::Image(sdk::PlacedImage { x: 0, y: 0, image }) }
}

fn blend(b: m::Blend) -> sdk::BlendMode {
    use m::Blend::*;
    match b {
        Normal => sdk::BlendMode::Normal,
        PassThrough => sdk::BlendMode::PassThrough,
        Multiply => sdk::BlendMode::Multiply,
        Screen => sdk::BlendMode::Screen,
        Overlay => sdk::BlendMode::Overlay,
        Darken => sdk::BlendMode::Darken,
        Lighten => sdk::BlendMode::Lighten,
        ColorDodge => sdk::BlendMode::ColorDodge,
        ColorBurn => sdk::BlendMode::ColorBurn,
        HardLight => sdk::BlendMode::HardLight,
        SoftLight => sdk::BlendMode::SoftLight,
        Difference => sdk::BlendMode::Difference,
        LinearDodge => sdk::BlendMode::LinearDodge,
        Hue => sdk::BlendMode::Hue,
        Saturation => sdk::BlendMode::Saturation,
        Color => sdk::BlendMode::Color,
        Luminosity => sdk::BlendMode::Luminosity,
        Other => sdk::BlendMode::Other,
    }
}

fn texts(entries: Vec<(String, String)>) -> Vec<sdk::MetaEntry> {
    entries.into_iter().map(|(key, v)| sdk::MetaEntry { key, value: sdk::MetaValue::Text(v) }).collect()
}

fn layer(l: m::Layer) -> sdk::Layer {
    sdk::Layer {
        id: l.id,
        stable_id: l.stable_id,
        parent: l.parent,
        name: l.name,
        kind: match l.kind {
            m::Kind::Raster => sdk::LayerKind::Raster,
            m::Kind::Folder => sdk::LayerKind::Folder,
        },
        visible: l.visible,
        opacity: l.opacity,
        blend: blend(l.blend),
        clip_to_below: false,
        offset_x: l.offset_x,
        offset_y: l.offset_y,
        pixels: match l.pixels {
            m::Pixels::None => sdk::LayerPixels::None,
            m::Pixels::Ready(member) => sdk::LayerPixels::Ready(png(member)),
            m::Pixels::Deferred(token) => sdk::LayerPixels::Deferred(token),
        },
        meta: texts(l.meta),
        semantic: texts(l.semantic),
        write: if l.writable { sdk::WriteCapability::Pixels } else { sdk::WriteCapability::None },
    }
}

/// The writer's host: the source file and the `edit` resources, which the
/// shared code names by number (the edits' pixels first, then what
/// `composite-after-edit` and `thumbnail` made; blobs in the order made).
struct SdkWriteHost<'a, 'b> {
    src: &'a SourceFile<'b>,
    given: Vec<edit::Pixels>,
    made: Vec<edit::Pixels>,
    blobs: Vec<Option<edit::Blob>>,
}

impl SdkWriteHost<'_, '_> {
    fn pixels(&self, id: u32) -> &edit::Pixels {
        let i = id as usize;
        if i < self.given.len() {
            &self.given[i]
        } else {
            &self.made[i - self.given.len()]
        }
    }

    fn handle(&self) -> u32 {
        (self.given.len() + self.made.len()) as u32
    }
}

impl m::Host for SdkWriteHost<'_, '_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len)
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        SdkHost { src: self.src }.inflate(member, max_out)
    }
}

impl m::WriteHost for SdkWriteHost<'_, '_> {
    fn encode_png(&mut self, pixels: u32) -> Result<u32, String> {
        let blob = edit::encode_image(self.pixels(pixels), &sdk::ImageEncoding::PngV1)?;
        self.blobs.push(Some(blob));
        Ok((self.blobs.len() - 1) as u32)
    }

    fn blob_info(&self, blob: u32) -> (u64, u32) {
        let b = self.blobs[blob as usize].as_ref().expect("a blob not yet placed");
        (b.len(), b.crc32())
    }

    fn composite_after_edit(&mut self) -> Result<u32, String> {
        let px = edit::composite_after_edit()?;
        let id = self.handle();
        self.made.push(px);
        Ok(id)
    }

    fn thumbnail(&mut self, pixels: u32, max_side: u32) -> u32 {
        let px = edit::thumbnail(self.pixels(pixels), max_side);
        let id = self.handle();
        self.made.push(px);
        id
    }
}

/// The development plugin misbehaving on purpose (tmi-plugin-host's tests
/// of `tmiki plugin dev`): a stored `stack.xml` whose `<image>` carries
/// `tmi-test-dev-fault="..."` makes `read-structure` hang, panic, exit, print
/// to stdout, call a host function wrongly, or answer with a value that
/// does not fit the WIT.
#[cfg(feature = "dev")]
fn dev_fault(src: &SourceFile) {
    let head = src.read(0, 4096);
    let marker = b"tmi-test-dev-fault=\"";
    let Some(at) = head.windows(marker.len()).position(|w| w == marker) else { return };
    let rest = &head[at + marker.len()..];
    let what = String::from_utf8_lossy(&rest[..rest.iter().position(|&b| b == b'"').unwrap_or(0)]).into_owned();
    match what.as_str() {
        "hang" => std::thread::sleep(std::time::Duration::from_secs(3600)),
        "panic" => panic!("the test fault asked for a panic"),
        "exit" => std::process::exit(3),
        "stdout" => {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"hello from the plugin\n");
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
        "bad-handle" => {
            // pixels that were never given: tmi traps the import
            let _ = codecs::crc32(&sdk::Bytes::Span(sdk::Span { offset: u64::MAX, len: 1 }));
        }
        _ => {}
    }
}

fn write_error(e: m::WriteFail) -> sdk::WriteError {
    match e {
        m::WriteFail::Unsupported(s) => sdk::WriteError::Unsupported(s),
        m::WriteFail::Invalid(s) => sdk::WriteError::Invalid(s),
        m::WriteFail::Corrupt(s) => sdk::WriteError::Corrupt(s),
    }
}

impl Plugin for Ora {
    fn info() -> sdk::PluginInfo {
        let i = crate::info();
        sdk::PluginInfo {
            id: i.id.into(),
            version: i.version.into(),
            formats: vec![sdk::FormatInfo {
                name: i.format_name.into(),
                extensions: i.extensions.iter().map(|e| e.to_string()).collect(),
                mime: i.mime.map(String::from),
            }],
            probe_bytes: i.probe_bytes,
            can_write: i.can_write,
        }
    }

    fn probe(_name: &str, head: &[u8], _size: u64) -> sdk::Confidence {
        match crate::probe(head) {
            m::Confidence::No => sdk::Confidence::No,
            m::Confidence::Maybe => sdk::Confidence::Maybe,
            m::Confidence::Yes => sdk::Confidence::Yes,
        }
    }

    fn read_structure(src: &SourceFile) -> Result<sdk::Structure, sdk::ReadError> {
        #[cfg(feature = "dev")]
        dev_fault(src);
        let s = crate::read_structure(&SdkHost { src }).map_err(error)?;
        Ok(sdk::Structure {
            format: crate::info().format_name.into(),
            canvas: sdk::Canvas { width: s.width, height: s.height, dpi: s.dpi, profile: sdk::ColorProfile::None },
            layers: s.layers.into_iter().map(layer).collect(),
            composite: match s.composite {
                m::Composite::Image(member) => sdk::Composite::Image(png(member)),
                m::Composite::Render => sdk::Composite::Render(sdk::Compositor::GenericV1),
            },
            file_meta: vec![],
            context: s.context,
            warnings: s.warnings.into_iter().map(|(code, message)| sdk::Warning { code, message }).collect(),
        })
    }

    fn read_layer(_src: &SourceFile, context: &[u8], token: u32) -> Result<sdk::Bitmap, sdk::ReadError> {
        crate::read_layer(context, token).map(png).map_err(error)
    }

    fn decode_chunk(_codec_id: u32, _input: &[u8], _out_len: u32) -> Result<Vec<u8>, sdk::ReadError> {
        Err(sdk::ReadError::Unsupported("ora has no codec of its own".into()))
    }

    fn write_layers(src: &SourceFile, edits: Vec<sdk::LayerEdit>, pixels: Vec<edit::Pixels>) -> Result<sdk::WritePlan, sdk::WriteError> {
        let mut list = Vec::with_capacity(edits.len());
        for e in edits {
            if !e.props.is_empty() {
                return Err(sdk::WriteError::Unsupported("ora writes layer pixels only, not properties".into()));
            }
            let px = e.new_pixels.ok_or_else(|| sdk::WriteError::Invalid(format!("{}: no new pixels", e.stable_id)))?;
            if px as usize >= pixels.len() {
                return Err(sdk::WriteError::Invalid(format!("{}: pixels {px} is not given", e.stable_id)));
            }
            list.push(m::Edit { stable_id: e.stable_id, pixels: px });
        }
        let mut host = SdkWriteHost { src, given: pixels, made: Vec::new(), blobs: Vec::new() };
        let plan = crate::write::write_layers(&mut host, &list).map_err(write_error)?;
        let mut pieces = Vec::with_capacity(plan.pieces.len());
        for p in plan.pieces {
            pieces.push(match p {
                m::Piece::Source(s) => sdk::Piece::Source(sdk::Span { offset: s.offset, len: s.len }),
                m::Piece::Bytes(b) => sdk::Piece::Bytes(b),
                m::Piece::Blob(id) => {
                    let blob = host.blobs.get_mut(id as usize).and_then(Option::take);
                    sdk::Piece::Blob(blob.ok_or_else(|| sdk::WriteError::Invalid(format!("blob {id} placed twice")))?)
                }
            });
        }
        Ok(sdk::WritePlan { pieces, warnings: plan.warnings.into_iter().map(|(code, message)| sdk::Warning { code, message }).collect() })
    }
}
```

### `src/write.rs`

writer：zip を作り直す write-plan。

```rust
//! `writer.write-layers` (PLUGINS.md 7.3's write flow): the zip rebuilt
//! around new layer PNGs.
//!
//! - Every member the edit does not touch is copied as it is
//!   (`piece::source`: its local header, its data and whatever followed it
//!   up to the next member), so its bytes never pass through the plugin.
//! - An edited layer's `src` member becomes a stored member holding the PNG
//!   the host encodes (`edit.encode-image(px, png-v1)`): a new local header
//!   (`bytes`, the original's fields with method 0, the blob's CRC and
//!   length, no data descriptor, no extra field), then the blob.
//! - `mergedimage.png`, when there is one, is rebuilt the same way from
//!   `edit.composite-after-edit`, and `Thumbnails/thumbnail.png` from its
//!   256 px thumbnail (OpenRaster asks for both to show the document).
//! - The central directory is copied record by record with the local header
//!   offsets moved (and, for a rebuilt member, its method, CRC and sizes),
//!   then the end of central directory record with the directory's new
//!   offset (its comment kept).
//!
//! Writing the same pixels again gives the same bytes (the host's PNG
//! encoder is fixed, `png-v1`), and so does writing into a file this writer
//! already wrote.
//!
//! A test-only attribute of `<image>`, `tmi-test-write-fault`, makes the
//! writer misbehave so the host's checks can be seen to stop it
//! (`tmi-plugin-host`'s tests, the conformance scenario `plugins`):
//! `other-layer` also writes the first edit's pixels into another layer,
//! `ignore-edit` keeps the edited layers' old bytes, `bad-span` points a
//! piece past the end of the file.

use std::collections::HashMap;

use crate::model::*;
use crate::zip::Directory;

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn corrupt(m: impl Into<String>) -> WriteFail {
    WriteFail::Corrupt(m.into())
}

fn read_err(e: Error) -> WriteFail {
    match e {
        Error::NotThisFormat(m) | Error::Corrupt(m) => WriteFail::Corrupt(m),
        Error::Unsupported(m) | Error::TooLarge(m) => WriteFail::Unsupported(m),
    }
}

/// What replaces a member: its new data (a blob of the host).
struct Rebuilt {
    blob: u32,
    len: u32,
    crc: u32,
}

const MERGED: &str = "mergedimage.png";
const THUMBNAIL: &str = "Thumbnails/thumbnail.png";

/// `writer.write-layers`.
pub fn write_layers(host: &mut dyn WriteHost, edits: &[Edit]) -> Result<Plan, WriteFail> {
    if !crate::info().can_write {
        return Err(WriteFail::Unsupported("this build of the ora plugin reads only".into()));
    }
    let read = crate::read_all(host).map_err(read_err)?;
    let fault = read.fault.as_deref();
    let dir = Directory::open(host).map_err(read_err)?;
    let archive = crate::zip::Archive::open(host).map_err(read_err)?;
    let size = host.size();

    // what each member becomes, by its local header's offset
    let mut rebuilt: HashMap<u64, Rebuilt> = HashMap::new();
    let mut rebuild = |host: &mut dyn WriteHost, name: &str, pixels: u32| -> Result<(), WriteFail> {
        let local = archive.local(name).ok_or_else(|| corrupt(format!("{name} is not in the archive")))?;
        let blob = host.encode_png(pixels).map_err(|e| WriteFail::Invalid(format!("{name}: {e}")))?;
        let (len, crc) = host.blob_info(blob);
        let len = u32::try_from(len).map_err(|_| WriteFail::Unsupported(format!("{name}: over 4 GiB (zip64 is not written)")))?;
        rebuilt.insert(local, Rebuilt { blob, len, crc });
        Ok(())
    };
    let source_of = |sid: &str| read.sources.iter().find(|(s, _)| s == sid).map(|(_, src)| src.clone());
    for e in edits {
        let src = source_of(&e.stable_id).ok_or_else(|| WriteFail::Invalid(format!("no raster layer {}", e.stable_id)))?;
        if fault != Some("ignore-edit") {
            rebuild(host, &src, e.pixels)?;
        }
    }
    if fault == Some("other-layer") {
        if let (Some(first), Some((_, other))) = (edits.first(), read.sources.iter().find(|(s, _)| edits.iter().all(|e| &e.stable_id != s))) {
            rebuild(host, other, first.pixels)?;
        }
    }
    if !edits.is_empty() && archive.member(MERGED).is_some() {
        let composite = host.composite_after_edit().map_err(|e| WriteFail::Unsupported(format!("{MERGED}: {e}")))?;
        rebuild(host, MERGED, composite)?;
        if archive.member(THUMBNAIL).is_some() {
            let thumb = host.thumbnail(composite, 256);
            rebuild(host, THUMBNAIL, thumb)?;
        }
    }

    // the members in file order, each running to the next (or the directory)
    let mut starts: Vec<u64> = dir.entries.iter().map(|e| e.local).collect();
    starts.sort_unstable();
    starts.dedup();
    if starts.last().is_some_and(|&s| s >= dir.cd_offset) {
        return Err(corrupt("a local header at or past the central directory"));
    }
    let mut pieces = Vec::new();
    let mut at: u64 = 0; // the written file's length so far
    let mut moved: HashMap<u64, u64> = HashMap::new(); // old local offset -> new
    let push_source = |pieces: &mut Vec<Piece>, at: &mut u64, offset: u64, len: u64| {
        if len > 0 {
            pieces.push(Piece::Source(Span { offset, len }));
            *at += len;
        }
    };
    let first = starts.first().copied().unwrap_or(dir.cd_offset);
    push_source(&mut pieces, &mut at, 0, first);
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(dir.cd_offset);
        moved.insert(start, at);
        match rebuilt.get(&start) {
            None => push_source(&mut pieces, &mut at, start, end - start),
            Some(r) => {
                let head = host.read(start, 30);
                if head.len() < 30 || head[..4] != *b"PK\x03\x04" {
                    return Err(corrupt("a member without a local header"));
                }
                let n = u16le(&head, 26) as usize;
                let name = host.read(start + 30, n as u32);
                let mut h = head.clone();
                put16(&mut h, 6, u16le(&head, 6) & !0x0008); // no data descriptor
                put16(&mut h, 8, 0); // stored
                put32(&mut h, 14, r.crc);
                put32(&mut h, 18, r.len);
                put32(&mut h, 22, r.len);
                put16(&mut h, 28, 0); // no extra field
                h.extend_from_slice(&name);
                at += h.len() as u64;
                pieces.push(Piece::Bytes(h));
                pieces.push(Piece::Blob(r.blob));
                at += r.len as u64;
            }
        }
    }
    let cd_start = at;
    let mut cd = Vec::with_capacity(dir.cd_size as usize);
    for e in &dir.entries {
        if e.record.offset + e.record.len > dir.cd_offset + dir.cd_size {
            return Err(corrupt(format!("{}: its central directory entry runs past the directory", e.name)));
        }
        let mut rec = host.read(e.record.offset, e.record.len as u32);
        let new_local = *moved.get(&e.local).ok_or_else(|| corrupt("a member the directory does not place"))?;
        let new_local = u32::try_from(new_local).map_err(|_| WriteFail::Unsupported("over 4 GiB (zip64 is not written)".into()))?;
        put32(&mut rec, 42, new_local);
        if let Some(r) = rebuilt.get(&e.local) {
            let flags = u16le(&rec, 8);
            put16(&mut rec, 8, flags & !0x0008);
            put16(&mut rec, 10, 0);
            put32(&mut rec, 16, r.crc);
            put32(&mut rec, 20, r.len);
            put32(&mut rec, 24, r.len);
        }
        cd.extend_from_slice(&rec);
    }
    at += cd.len() as u64;
    let mut eocd = host.read(dir.eocd.offset, dir.eocd.len as u32);
    let cd_start32 = u32::try_from(cd_start).map_err(|_| WriteFail::Unsupported("over 4 GiB (zip64 is not written)".into()))?;
    put32(&mut eocd, 12, cd.len() as u32);
    put32(&mut eocd, 16, cd_start32);
    pieces.push(Piece::Bytes(cd));
    pieces.push(Piece::Bytes(eocd));
    let _ = at;
    if fault == Some("bad-span") {
        pieces.push(Piece::Source(Span { offset: size, len: 10 }));
    }
    Ok(Plan { pieces, warnings: Vec::new() })
}
```

### `src/bin/ora-dev.rs`

開発用プラグイン（`tmiki plugin dev`）。

```rust
//! The OpenRaster plugin as a development plugin: the same code as the
//! component, run natively over `tmiki plugin dev`'s RPC.
//!
//!     cargo build -p tmi-plugin-ora --features dev --bin ora-dev
//!     tmiki plugin dev target/debug/ora-dev -- add a.ora

fn main() {
    tmi_plugin_sdk::dev_main::<tmi_plugin_ora::plugin::Ora>()
}
```

## 11. 開発用プラグイン（`tmiki plugin dev`）と SDK

component を作り直さずに試し、ネイティブのデバッガや `eprintln!` で調べるために、同じコードを**開発用プラグイン**（普通のプログラム）としても動かせる。開発用プラグインは標準入出力でホストと 1.1 の WIT の値をやりとりする。`tmiki plugin new` の雛形は、そのための SDK（`tmi-plugin-sdk`）の写しを `sdk/` に持っている（tmi が書いたもので、直さない）。

```
tmiki plugin build                         component と開発用プラグインを作る
tmiki plugin test --native                 tmi-plugin.toml の [dev] command を動かし、component と同じ答えか確かめる（「1 native = wasm」）
tmiki plugin test --native-command "<command>"  別のコマンドで（語を空白で区切る。空白を含む語は "..."。JSON の文字列の配列でもよい）
tmiki plugin dev <program> [<arg>...]      開発用プラグインを起動して info を見せる（リポジトリの中なら .tmiattributes の対応も）
tmiki plugin dev <program> [<arg>...] -- add a.ext   その tmi のコマンドを、.tmiplugins/<id>.wasm の代わりに開発用プラグインで動かす
```

- 開発用プラグインが作った記録は `importer.component_sha256 = "dev"` を持ち、`commit`（`--amend`・マージのコミットも）と `stash` が断る。本番の記録は component を `tmiki plugin add` してから取り込み直す（`tmiki plugin reimport`）。
- 開発用プラグインはサンドボックスの外で動く利用者自身のプログラムなので、信頼（`plugin.trust`）と書き戻しの同意は聞かれない。決定性も保証されない（1 つのプロセスがすべての呼び出しに答える）。1 回の呼び出しの時間の上限（`plugin.timeout`、既定 120 秒）だけがかかり、超えるとプロセスを止めて次の呼び出しで起動し直す。
- 環境変数 `TMI_PLUGIN_DEV`：プログラムのパス、またはプログラムと引数の JSON の配列（`["target/debug/kra-dev", "-v"]`）、または複数のプラグインの配列の配列。`tmiki plugin dev ... --` はこれを付けてコマンドを動かす。
- **他人のプラグインのディレクトリで `build`・`test --native` を打つのは、そのコードを実行すること**。`tmiki plugin build` は cargo を動かす（`build.rs`・マクロ・`.cargo/config.toml` はそのディレクトリの作者のコード）。`tmiki plugin test --native`（`--native-command` なし）は tmi-plugin.toml の `[dev] command`（ディレクトリに同梱された実行ファイルでもよい）をサンドボックスなしで動かす。clone したばかりのディレクトリで打つ前に中身を読む。tmi は、そのディレクトリ（実パス）と中身（build：tmi-plugin.toml の内容、`[dev] command`：コマンドと同梱プログラムのハッシュ）ごとに 1 度だけ同意を求め、`trust.json` の `directories` に覚える（内容が変わると聞き直す）。端末が無いときは断る。許すには、端末で 1 度打つか、そのコマンドだけ `TMI_PLUGIN_DIR_TRUST=allow` を付ける。自分が `tmiki plugin new` で作ったディレクトリと、自分が許した `build` が作った開発用プラグインは聞かれない。`--native-command` と `plugin dev <program>` は自分で打つコマンドなので聞かれない。tmi-plugin.toml の `build.output`・`test.corpus` はディレクトリの内側の相対パスだけ（`..`・絶対パス・ドライブ・`:` は断る）。
- **stdout はホストへの管**。開発用プラグインが stdout に何か書くと、ホストは「not a frame」で呼び出しを失敗にする。`println!` を使わず `eprintln!` を使う。stderr の行は `tmiki: plugin <id> (dev): ...` と表示される。

**Rust**（`tmi-plugin-sdk`）：WIT の型（wit-bindgen 0.63 のもの）をそのまま使い、trait `tmi_plugin_sdk::Plugin` を実装する。`info`・`probe`・`read_structure` は必須、`read_layer`・`decode_chunk`・`write_layers` は既定で `unsupported` を返す。ホストの関数は `SourceFile`（`size`・`read`・`name`）、`codecs::{transform, crc32, sha256, image_info}`、`sqlite::Database`、`xml::parse`、`log::emit`、`edit::{Pixels, Blob, encode_tile, encode_image, composite_after_edit, thumbnail}`。書き戻しの答えは `WritePlan { pieces: Vec<Piece>, warnings }`（`Piece::Source(Span)`・`Bytes(Vec<u8>)`・`Blob(edit::Blob)`）。

```rust
use tmi_plugin_sdk::{self as t, SourceFile};
pub struct Plugin;
tmi_plugin_sdk::export_plugin!(Plugin);          // wasm32 でだけ component の export になる
impl t::Plugin for Plugin {
    fn info() -> t::PluginInfo { /* ... */ }
    fn probe(name: &str, head: &[u8], size: u64) -> t::Confidence { /* ... */ }
    fn read_structure(src: &SourceFile) -> Result<t::Structure, t::ReadError> { /* ... */ }
}
// src/bin/dev.rs（feature dev）：fn main() { tmi_plugin_sdk::dev_main::<my_crate::Plugin>() }
```

`cargo build --release --target wasm32-wasip2` が component、`cargo build --features dev` が `target/debug/<id>-dev`（`tmiki plugin build` は両方を作る）。

### 11.1 RPC（自分で開発用プラグインを書く場合）

SDK を使えば知らなくてよい。SDK を使わずに開発用プラグインを書くときの約束（配線は言語に依らない。crate `tmi-plugin-dev` の doc が正本）：

- 1 つのメッセージは 1 つのフレーム：4 バイトのビッグエンディアンの長さ `n`（1〜1 GiB）と、ちょうど 1 つの CBOR の値（RFC 8949。長さの決まった形だけ、タグは使わない）。tmi はプラグインの stdin に書き、stdout から読む。
- WIT の値の CBOR：`bool` は true/false、整数は整数、`f64` は浮動小数（整数でも受ける）、`string` は text、`list<u8>` は byte string、その他の `list`・`tuple` は配列、`option` は null か値、`record` は WIT の綴りのフィールド名（`stable-id`）をキーにした map、`enum` はケース名の text（`pass-through`）、`variant` はケース名から中身への 1 項目の map（中身の無いケースは null：`{"deflate": null}`、`{"span": {"offset": 0, "len": 9}}`）、`result` は `{"ok": 値}` か `{"err": 値}`、resource はハンドルの番号。
- メッセージ（どれも map）：`{"call": 名前, "args": [...]}`（tmi がプラグインの export を呼ぶ。名前は `reader.info`・`reader.probe`・`reader.read-structure`・`reader.read-layer`・`reader.decode-chunk`・`writer.write-layers` と `hello`。引数は WIT の順、`borrow<source-file>` はハンドル）、`{"import": 名前, "args": [...]}`（プラグインがホストの関数を呼ぶ：`codecs.transform`、resource のメソッドはハンドルを先頭に `source.source-file.read` `[h, offset, len]`、静的な関数は `sqlite.database.open` `[image]`、`<interface>.<resource>.drop` `[h]`）、`{"return": 値}`、`{"trap": 文}`。
- 会話は入れ子：call のあと、プラグインは import をいくつでも送り（それぞれの答えを待つ）、最後に return か trap を 1 つ送る。tmi が import に trap で答えたら、プラグインはその call を trap で終える。ハンドルは 1 回の call の中だけで有効：元ファイルは 0、`write-layers` の画素は `0..n`、データベースは開いた順。
- 最初の call は `hello`（`args: [1]`、プロトコルの版）。答えは `{"protocol": 1, "abi": "tmi:format@0.1.0", "writer": bool}`（`writer`：writer を export するか。書き戻しには `info` の `can-write` も要る）。stdin が閉じたら終わる。
- `tmiki plugin test` の「1 native = wasm」は、component の答えと開発用プラグインの答え（deferred を読んだ構造・記録・警告）を、この写し方の値にして**正規の CBOR**（RFC 8949 4.2.1：整数と長さは最短、浮動小数は値を変えない最短の幅、map は キーの符号の順）で比べる。違えば、最初に違う所を `layers[2].opacity: 0.5 vs 0.25` のように示す。
