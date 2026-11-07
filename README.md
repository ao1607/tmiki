# tmiki — イラストのための Git

**tmiki is Git for layered illustration files (CLIP STUDIO PAINT `.clip`, MediBang / FireAlpaca `.mdp`).**
**It tracks layers, merges per layer, shows diffs as pictures, and attaches a thumbnail to every commit.**
**Commands are Git's own (`init` / `add` / `commit` / `log` / `diff` / `branch` / `merge` …). Docs are in Japanese.**

**「あの時のレイヤー、もう一回見たい」を、ちゃんと取り出せるバージョン管理。**

tmiki（ツミキ）は、CLIP / MDP2 などの **レイヤーのあるイラストファイル** を、Git のように履歴管理するためのツールです。
1 枚絵の制作・連載・差分提出・複数人での共同作画など、「途中の状態をなくしたくない」場面に使えます。

> 開発中のソフトウェアです。まだリリース版（配布用の実行ファイル）はありません。いまはソースからビルドして使います。

---

## こんなことに困っていませんか？

- `illust_v3_final_FIX2_本当の最終.clip` を量産してしまう
- 「2日前の塗りの方が良かった」けど上書き保存済み
- 共同作画で、相手の修正と自分の修正が混ざってどっちが正解か分からない
- どこをどう直したのか、後から自分でも思い出せない
- 過去のレイヤーだけ1枚 PNG で抜きたいのに、もうファイルがない

tmiki はこれらを、**Git と同じ考え方** で、しかも **レイヤー単位** で解決します。

---

## できること

### レイヤーごとの履歴

`.clip`（CLIP STUDIO PAINT）/ `.mdp`（メディバンペイント・FireAlpaca）を **レイヤー構造のまま** 保存します（PSD のレイヤーは今後の予定）。
コミットしたあとからでも、「あの時の "線画" レイヤーだけ PNG で取り出す」「"影" レイヤーだけ昔の状態に戻す」ができます。

```
tmiki restore --source HEAD~3 --output line_old.png illust.clip:line
```

### ビジュアル diff

差分が「何バイト変わった」ではなく **絵で見える** のが tmiki の中心の機能です。

- side-by-side: 前後を並べる
- overlay: 重ねて変更箇所をハイライト
- per-layer: レイヤー別に差分を出す
- HTML レポート: 1 ファイルにまとめてブラウザでめくれる

```
tmiki diff --html illust.clip
```

比べる相手は Git と同じです。引数なしの `tmiki diff` は「まだ `add` していない変更」、`add` した後は `tmiki diff --staged`、コミット済みの変更は `tmiki diff HEAD~1` です。

### レイヤーを見分ける 3-way マージ

ブランチを切って別バージョンを作り、後でマージできます。
**同じファイルでも別のレイヤーをいじっていれば自動でマージできます。** 片側だけのレイヤー追加・削除・名前の変更・不透明度などの変更も自動で取り込みます（両側がレイヤー構成を変えた場合は衝突）。
ぶつかったレイヤーだけが「衝突」として上がってくるので、`tmiki restore --theirs illust.clip:線画` のようにレイヤー単位で選べます。

### サムネが必ず付くコミット履歴

コミットには **その時点の合成サムネが必ず添付されます**。`tmiki show --thumbnail <コミット>` で PNG として取り出せます。

### Git のコマンドそのまま

`init` / `add` / `commit` / `log` / `diff` / `branch` / `switch` / `merge` / `restore` / `tag` / `remote` / `fetch` / `pull` / `push` / `clone`。
Git を触ったことがあれば、ほぼそのまま使えます。

### どんなファイルでも一応バージョン管理できる

レイヤーを読めるのは CLIP / MDP2 だけですが、ほかのファイルも Git と同じように追跡できます。
PNG / JPEG / GIF / BMP / TIFF / WebP / PSD（合成画像）は画像として、それ以外はバイナリとして格納します。

### 形式プラグイン

対応形式は WebAssembly のプラグイン（Rust で書く）で増やせます。仕様は [`rust/crates/tmi-plugin-tools/PLUGIN_SPEC.md`](rust/crates/tmi-plugin-tools/PLUGIN_SPEC.md)、雛形は `tmiki plugin new` で作れます。

---

## 使い方の例

```powershell
# はじめる
tmiki init
tmiki add illust.clip
tmiki commit -m "ラフ"

# 進めてコミット
tmiki commit -a -m "線画"
tmiki commit -a -m "下塗り"

# 未コミットの今のファイル、どう変わった？
tmiki diff --html illust.clip   # ブラウザで開く

# 試行錯誤用にブランチ
tmiki switch -c try-purple
# ...いろいろ塗る...
tmiki commit -a -m "紫案"

# 気に入らなければ戻る
tmiki switch main
tmiki branch -D try-purple   # 案を捨てる

# 気に入ればマージ
tmiki merge try-purple -m "紫案を採用"
```

---

## インストール（ソースから）

リリース版の実行ファイルはまだありません。いまは次の手順でソースから入れます。Python 3.10 以上、[Rust](https://rustup.rs)、Windows では Visual Studio Build Tools の C++ が要ります。

```powershell
pip install .\rust\tmi-accel        # 描画などのネイティブモジュール
pip install .\rust\crates\tmi-cli   # コマンド tmiki（Rust の実行ファイル）
pip install -e .                    # Python 側の本体
tmiki --version
```

Rust のビルドに数分かかります（その間 pip は何も表示しません）。PyPI には公開していないので、`pip install tmiki` のように名前で入れないでください。

詳しい使い方・部品の役割・困ったときの切り分けは [docs/MANUAL.md](docs/MANUAL.md) にあります。

---

## どこまでできる？ どこは未対応？

**入っているもの**
- Git のメインライン相当のコマンド一式
- `.clip` / `.mdp`（MDP2）のレイヤーの読み込みと書き戻し
- レイヤー単位の add / restore / merge
- ビジュアル diff（side-by-side / overlay / per-layer / HTML）
- 合成サムネを自動で添付するコミット
- フック（`pre-commit` ほか）と形式プラグイン
- 履歴の復旧（`recover` / `reflog`）と `gc`
- ローカルパス・bare リポジトリ・HTTPS リモートへの push / pull / clone

**今後の予定**
- 配布用の実行ファイル（Windows / macOS / Linux）
- PSD のレイヤー認識
- 大きなファイルを必要になってから取得する遅延 fetch（LFS 相当）
- GUI クライアント

---

## 誰のためのもの？

- 1 人で描いていて、過去のレイヤー状態をなくしたくない人
- 「前回最終 → 今回原稿」の差分を出したい人
- 共同作画で、誰がどのレイヤーを触ったかを正しくマージしたい人
- 「Git をイラストでもやりたかった」人

---

## 名前について

tmiki（ツミキ）は、レイヤーとコミットを積み上げる道具という意味です。開発中は tmi という名前だったので、Python のパッケージ・Rust の crate・リポジトリの中身（`.tmi/`）・環境変数（`TMI_*`）・設計の記録（`RUST_MIGRATION.md`・`PLUGINS.md` など）には旧名が残っています。

## ファイル形式について

CLIP / MDP2 の読み書きは、各アプリで条件を変えて保存したファイルを観察して実装しています。アプリ本体の解析はしていません。調べた内容は [`research/`](research/) にあります。

tmiki は個人の独立したプロジェクトで、CLIP STUDIO PAINT・メディバンペイント・FireAlpaca の開発元とは関係ありません。各名称は各社の商標です。

## ライセンス

[MIT](LICENSE-MIT) または [Apache-2.0](LICENSE-APACHE) のどちらかを選べます。`rust/vendor/libsqlite3-sys` は同梱した外部のコードで、それぞれのライセンスに従います。
