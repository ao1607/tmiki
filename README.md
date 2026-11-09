<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/tmiki-lockup-dark.svg">
    <img src="docs/brand/tmiki-lockup-light.svg" alt="tmiki" width="340">
  </picture>
</p>

<h3 align="center">レイヤーごとに、絵の履歴を。</h3>
<p align="center">CLIP STUDIO PAINT とメディバンペイントのファイルを、Git と同じ操作でバージョン管理。</p>

<p align="center">
  <a href="#できること">できること</a> · <a href="#インストール"><b>インストール</b></a> ·
  <a href="#対応形式">対応形式</a> · <a href="#使い方">使い方</a> ·
  <a href="#ヘルプ">ヘルプ</a>
</p>

<p align="center">
  <a href="#ライセンス"><img src="https://img.shields.io/badge/license-AGPL--3.0-4A3B32" alt="AGPL-3.0"></a>
  <img src="https://img.shields.io/badge/status-pre--release-E2B77A" alt="リリース前">
  <a href="https://github.com/ao1607/tmiki/commits/main"><img src="https://img.shields.io/github/last-commit/ao1607/tmiki?color=6C9BD2" alt="最終更新"></a>
</p>

<p align="center"><sub><b>日本語</b> · <a href="README.en.md">English</a></sub></p>

**無料でオープンソース。** 描くアプリはいつものまま。保存したファイルを `tmiki commit` で記録すると、レイヤー単位の差分・マージ・巻き戻しができます。アカウントもサーバーも要りません。

<p align="center">
  <img src="docs/media/diff-side-by-side.png" width="900" alt="tmiki diff --html の報告。左が前の版、右が今の版で、変わったレイヤー（塗り・線画）が表に並ぶ">
  <br><sub>前の版と今の版を並べて、どのレイヤーが変わったかまで見られます（<code>tmiki diff --html</code>）。</sub>
</p>

- `illust_v3_final_FIX2_本当の最終.clip` を量産してしまう
- 「2 日前の塗りの方が良かった」のに上書き保存してしまった
- 共同作画で、相手の修正と自分の修正がどちらが正解か分からなくなる
- 過去のレイヤーを 1 枚だけ PNG で取り出したいのに、もうファイルがない

tmiki はこれらを、Git と同じ考え方で、しかもレイヤー単位で解決します。

## できること

<details>
<summary><b>レイヤーごとの履歴</b> — 過去の 1 レイヤーだけを取り出す・戻す</summary>
<br>

`.clip` と `.mdp` をレイヤー構造のまま記録します。コミットしたあとからでも、「3 つ前の "線画" だけ PNG で取り出す」「"影" だけ昔の状態に戻す」ができます。

```powershell
tmiki restore --source HEAD~3 --output line_old.png illust.clip:線画
tmiki restore --source HEAD~3 --in-place illust.clip:影
```

</details>

<details>
<summary><b>絵で見る diff</b> — 並べる・重ねる・レイヤー別・スワイプ</summary>
<br>

差分は「何バイト変わったか」ではなく絵で出ます。`--html` で、ブラウザでめくれる報告を 1 ファイルにまとめます。

<p align="center">
  <img src="docs/media/diff-per-layer.png" width="560" alt="per-layer 表示。変わった範囲が赤い枠で示される">
  <br><sub>per-layer 表示。変わった範囲を枠で示します。</sub>
</p>

比べる相手は Git と同じです。`tmiki diff` はまだ `add` していない変更、`--staged` は `add` した変更、`tmiki diff HEAD~1` はコミット済みの変更です。

</details>

<details>
<summary><b>レイヤーを見分けるマージ</b> — 別々のレイヤーを描いたら、そのまま 1 枚に</summary>
<br>

ブランチで別の案を描いて、あとから取り込めます。同じファイルでも、触ったレイヤーが違えば自動でマージされます。両方が同じレイヤーを描き換えたときだけ、そのレイヤーが衝突として上がり、`tmiki restore --theirs illust.clip:線画` のようにレイヤー単位で選べます。

<p align="center">
  <img src="docs/media/terminal-merge.png" width="620" alt="main で屋根の色を変え、flag ブランチで旗を描き、merge で自動的に 1 枚にまとまった端末の出力">
  <br><sub>main は「塗り」、flag ブランチは「線画」を描いたので、衝突なしでまとまります。冒頭の画像の右側がこの結果です。</sub>
</p>

</details>

<details>
<summary><b>サムネが付く履歴</b> — ハッシュではなく絵で思い出す</summary>
<br>

コミットには、その時点の合成サムネが必ず付きます。`tmiki show --thumbnail <コミット>` で PNG として取り出せます。

</details>

<details>
<summary><b>Git のコマンドそのまま</b> — 覚え直しはいりません</summary>
<br>

`init` · `add` · `commit` · `status` · `log` · `diff` · `branch` · `switch` · `merge` · `restore` · `tag` · `stash` · `remote` · `fetch` · `pull` · `push` · `clone`。用語も Git のままです。ローカルのフォルダ、bare リポジトリ、HTTPS のリモートとやりとりできます。

</details>

<details>
<summary><b>形式プラグイン</b> — 対応形式を自分で足す</summary>
<br>

対応形式は WebAssembly のプラグイン（Rust で書きます）で増やせます。`tmiki plugin new` で雛形を作り、`tmiki plugin test` で確かめます。仕様は [PLUGIN_SPEC.md](rust/crates/tmi-plugin-tools/PLUGIN_SPEC.md) にあります。

</details>

## インストール

**リリース版（配布用の実行ファイル）はまだありません。** いまはソースからビルドして使います。

<details open>
<summary><b>ソースからビルドする</b></summary>
<br>

必要なもの：Python 3.10 以上、[Rust](https://rustup.rs)、Windows では Visual Studio Build Tools の C++。

```powershell
git clone https://github.com/ao1607/tmiki
cd tmiki
pip install .\rust\tmi-accel        # 描画などのネイティブモジュール
pip install .\rust\crates\tmi-cli   # コマンド tmiki
pip install -e .                    # Python 側の本体
tmiki --version
```

Rust のビルドに数分かかります（その間 pip は何も表示しません）。PyPI には公開していないので、`pip install tmiki` のように名前では入れないでください。

</details>

[利用の手引き（全文）](docs/MANUAL.md) · [困ったとき](docs/MANUAL.md#困ったとき)

## 対応形式

| 形式 | 記録のしかた | diff | マージ |
| --- | --- | --- | --- |
| **CLIP STUDIO PAINT** `.clip` | レイヤーごと | レイヤー別まで | レイヤー単位で自動 |
| **メディバンペイント・FireAlpaca** `.mdp` | レイヤーごと | レイヤー別まで | レイヤー単位で自動 |
| **PNG · JPEG** | 1 枚の画像 | 絵で比較 | 自動 |
| **GIF · BMP · TIFF · WebP · PSD**（合成画像） | 1 枚の画像 | 絵で比較 | 両方で変えたらどちらかを選ぶ |
| **そのほか** | バイナリ | 変わったかどうか | どちらかを選ぶ |

PSD のレイヤーは今後の予定です。どれに入るかは拡張子ではなく中身で決まります。[詳しく](docs/MANUAL.md#対応フォーマット)

## 使い方

```powershell
tmiki init
tmiki add illust.clip
tmiki commit -m "ラフ"

tmiki diff --html illust.clip      # まだ記録していない変化をブラウザで見る
tmiki commit -a -m "線画"          # 描き進めたら、そのたびに

tmiki switch -c try-purple         # 別の案はブランチで
tmiki commit -a -m "紫案"
tmiki switch main
tmiki merge try-purple -m "紫案を採用"
```

マージや巻き戻しで tmiki がファイルを書き換えたあと、描くアプリで開いたまま保存すると、古い内容に戻ってしまいます。開き直してから続けてください。tmiki も書き換えたファイルの一覧を出して知らせます。

## ヘルプ

[利用の手引き](docs/MANUAL.md) · [プラグインの作り方](rust/crates/tmi-plugin-tools/PLUGIN_SPEC.md) · [不具合を報告](https://github.com/ao1607/tmiki/issues/new) ·
[ファイル形式の調査メモ](research/) · [設計の記録](PLUGINS.md)

## このプロジェクトについて

**名前**：tmiki（ツミキ）は、レイヤーとコミットを積み上げる道具という意味です。開発中は tmi という名前だったので、Python のパッケージ・Rust の crate・リポジトリの中身（`.tmi/`）・環境変数（`TMI_*`）・設計の記録には旧名が残っています。

**商標**：tmiki は個人の独立したプロジェクトで、CLIP STUDIO PAINT・メディバンペイント・FireAlpaca の開発元とは関係ありません。各名称は各社の商標です。

## ライセンス

tmiki は [GNU AGPL-3.0](LICENSE) です。誰でも無料で使え、仕事の絵を描くのに使うのも自由です。改造したものを配ったり、ネット越しのサービスとして提供したりするときは、そのソースを同じ AGPL-3.0 で公開する必要があります。

プラグインを作るための部品は [MIT](LICENSE-MIT) です。`tmiki plugin new` が書き出すファイル一式と、その元（`rust/plugins/sdk`・`rust/crates/tmi-plugin-dev`・`rust/crates/tmi-plugin-tools/templates`・`rust/crates/tmi-format/wit`・PLUGIN_SPEC.md）が対象で、プラグインには好きなライセンスを付けられます。`rust/vendor/libsqlite3-sys` は同梱した外部のコードで、それぞれのライセンスに従います。

<sub>AI ツール向け：<a href="CLAUDE.md">開発の手引き（英語）</a></sub>
