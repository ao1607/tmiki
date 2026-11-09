# tmiki 利用の手引き

> tmiki は開発中の旧名を tmi といいます。コマンド名は `tmiki` ですが、Python のパッケージ (`python -m tmi`)・Rust の crate (`tmi-*`)・リポジトリの中身 (`.tmi/`・`.tmiattributes`)・環境変数 (`TMI_*`) は `tmi` のままです。

レイヤー認識の3-wayマージ・ビジュアル diff・サムネ必須コミットを備えた、イラストファイル (CLIP / MDP2 / PNG / JPEG) 向けの Git ライクなバージョン管理システム。

用語は Git そのままです (commit / branch / tag / merge / log / HEAD / remote …)。

## インストール

tmiki は単一の実行ファイル（`tmiki.exe`）です。[Releases](https://github.com/ao1607/tmiki/releases) のページからダウンロードするか、Rust でソースからビルドしてください。Python は不要です。

```powershell
cd rust
cargo build --release -p tmi-cli   # rust\target\release\tmiki.exe
```

### 開発用の Python パッケージ

リポジトリにはPythonパッケージ（`tmi`）とリファレンス実装（`tmiref/`）も含まれています。これらは開発用で、テストでは実行ファイルと照合します。`TMI_PYTHON` にパッケージがインストール済みのPythonを指定すると、実行ファイルは自身で処理できない稀なコマンドラインをそのPythonに渡します。`TMI_PYTHON` がない場合、そのようなコマンドラインは「not supported」とその理由とともに停止します。

テストとリファレンス実装には numpy、Pillow、`zstandard` (`pip install -e .[test]`) が必要であり、Python パッケージにはネイティブモジュール (`pip install .\rust\tmi-accel`) が必要です。

形式プラグインのホスト (`rust\crates\tmi-plugin-host`、PLUGINS.md の P5) は wasmtime ごと `tmiki` の実行ファイルに入っています。`tmi-cli` のビルドに追加の手順はありませんが、Rust のテスト (`cargo test --workspace`) はテスト用のプラグイン (`rust\plugins`。独立した workspace) を WebAssembly にビルドして使うので、先に `rustup target add wasm32-wasip2` が要ります。手で作るときは `cargo build --release --target wasm32-wasip2 --manifest-path rust\plugins\Cargo.toml` です。

### 困ったとき

次の環境変数で、tmi の動きを切り分けられます。

| 変数 | 働き |
| --- | --- |
| `TMI_DEBUG_FORWARD=1` | `TMI_PYTHON` が設定されている場合：コマンドを Python に渡した理由を1行で出力します |
| `TMI_NATIVE=0` | `TMI_PYTHON` が設定されている場合：すべてのコマンドを Python で実行します（実行ファイルの結果と比較するため） |
| `TMI_PYTHON=<python のパス>` | 開発用：サポート対象外のコマンドラインを渡すためのPython（これがないとtmikiはPythonを使用しません） |
| `TMI_PROGRESS=1` / `0` | 進捗を端末以外にも出す / 端末でも出さない (下記) |
| `TMIHUB_TIMEOUT=<秒>` | HTTPS リモート (tmihub) との通信で、接続・送信・受信のそれぞれが進まないまま待つ秒数 (既定 300)。超えると「tmihub stopped answering」で止まります。少しずつでも進んでいる転送は、どれだけ長くても切りません |

バグレポートに`tmiki --version`の出力結果を添付してください。

### add が止まったとき

- `tmiki add .` は、読み込めないファイル (画素数の上限 `core.maxImagePixels` を超えた画像など) があっても残りを追加し、最後に `tmiki: error: 2 files could not be added:` とファイル名つきの一覧を出して、終了コード 1 で終わります。画像として読めないファイル (壊れた画像など) は、エラーではなく警告つきで不透明バイナリとして記録します (上の「対応フォーマット」)。
- 形式プラグインが読めないファイル (保存の途中で切れた .ora など) も同じです: そのファイルは記録せずに一覧に出し、残りは追加します (`add --renormalize`・`plugin reimport` も同じ)。`commit -a`・`mv`・`stash push` は、読めないファイルが 1 つでもあれば commit・移動・退避をせずに止まります (一部だけの commit を作らないため)。
### 進捗の表示

`add` / `commit -a` / `status` / `diff` / `merge` / `stash` / `switch` / `checkout` / `restore` / `reset --hard` / `clone` / `fetch` / `push` / `pull` / `gc` などの長い処理では、いまの段階 (`parsing` / `decoding layers` / `rendering composite` / `rendering thumbnail` / `writing layer` など) と経過時間が端末に表示されます (標準出力と標準エラー出力の両方が端末のとき)。端末以外 (ログへのリダイレクトなど) にも出したい場合は `TMI_PROGRESS=1` (段階ごとの所要時間も最後に出ます)、端末でも消したい場合は `TMI_PROGRESS=0` を設定してください。

### ディスクの空きが足りないとき

どのファイルを記録中だったかと、必要な量・空きの量を示して止まります。そのファイルのために書きかけたオブジェクトとキャッシュはその場で消します (すでにステージしたファイルは残ります)。空きを作ってもう一度実行してください。

## 対応フォーマット

どのファイルがどれに入るかは、拡張子ではなく中身で決まります (tmi 自身のデコーダが読めるかどうか。Pillow の有無や版には左右されません)。

- **レイヤー認識**: `.clip` (CLIP STUDIO PAINT) / `.mdp` (MDP2: メディバンペイント・FireAlpaca)。レイヤー単位の diff・マージ・復元ができます。`.psd` / `.kra` のレイヤー認識は未実装です (`.psd` は次の「画像として記録」、`.kra` は「その他」に入ります)
- **画像として記録**: tmi 自身のデコーダ (Rust の `tmi-image`) が最後まで読める PNG・JPEG・GIF・BMP / DIB・TIFF・WebP・PSD は、1 枚のラスタレイヤーを持つ画像として記録し、サムネとビジュアル diff が使えます。PSD は統合画像 (合成結果) だけを読み、レイヤーは認識しません。アニメーション PNG (APNG) と MPO は 1 枚目です。自動マージできるのは PNG と JPEG だけで、GIF・BMP・TIFF・WebP・PSD は両方で変えると衝突になり、`--ours` / `--theirs` でどちらかを選びます (レイヤーが無いので `<file>:<layer>` の指定はできません)
- **その他**: 読めないものは、Git と同様にバージョン管理できる不透明バイナリとして格納します (差分は「バイナリが変わった」とだけ出て、元のバイト列のまま戻せます)。tmi にデコーダの無い形式 (TGA・PCX・ICO・`.kra` など)、デコーダが断る変種 (YCbCr や LAB の TIFF、I・F モード、LAB の PSD、JPEG 圧縮の TIFF など)、壊れた画像 (libjpeg が警告を出す JPEG、壊れた MPO、途中で切れた PNG・JPEG・GIF・WebP、0 バイトや中身が違うファイル) がこれに入ります。画像らしいファイル (画像の拡張子、または画像の先頭バイト) が読めなかったときは、`tmiki: warning: could not read b.jpg as a JPEG image (...); tracking as opaque binary` と、ファイル名つきで警告します (保存やダウンロードの途中でないか確かめてください)

シンボリックリンク (Windows ではジャンクションも) は記録しません。`tmiki add link.png` のように名前を指定するとエラーになり、`tmiki add .` ではリンクごとに警告して飛ばします (リンク先の実ファイルは、実際のパスで追跡されます)。

## 基本的な流れ

```powershell
# リポジトリ作成
tmiki init

# ファイルをステージしてコミット (合成サムネは自動添付)
tmiki add illust.clip
tmiki commit -m "初稿"

# ステージを取り消す (インデックスを HEAD に戻す。作業ツリーの変更は残る)
tmiki restore --staged illust.clip

# 状態を見る
tmiki status
tmiki status -sb
tmiki log --graph --oneline --decorate --all
tmiki log --stat --name-status -n 5

# 差分をビジュアルに書き出す (Git と同じく: 引数なし=ステージ↔作業ツリー、--staged=HEAD↔ステージ、
# <rev>=そのコミット↔作業ツリー、<rev> <rev>=コミット同士)
tmiki diff --side-by-side --out diff_out illust.clip
tmiki diff --overlay       --out diff_out illust.clip
tmiki diff --per-layer     --out diff_out illust.clip
tmiki diff --name-status
tmiki diff --stat
tmiki diff --staged --stat
tmiki diff HEAD~1 HEAD --name-status

# HTML レポートを生成してブラウザで開く (縮小プレビューを埋め込み、原寸画像は同じフォルダにリンク)
tmiki diff --html illust.clip

# コミットの合成サムネを PNG で取り出す (-o を省略すると .tmi/thumbnails/ に書き出してパスを表示)
tmiki show --thumbnail HEAD~2 -o thumb.png
```

引数なしの `tmiki diff` は「ステージ↔作業ツリー」の比較なので、`add` した変更は出てきません (何も出ないのにステージ済みの変更があるときは、`--staged` を案内する1行を出します)。比べたい相手に応じて使い分けてください。

| 見たいもの | コマンド |
| --- | --- |
| まだ `add` していない変更 | `tmiki diff` |
| `add` した (次のコミットに入る) 変更 | `tmiki diff --staged` |
| 直前のコミットで変えたところ | `tmiki diff HEAD~1 HEAD` (作業中の変更も含めるなら `tmiki diff HEAD~1`) |

`--out` を省略すると `.tmi/diff/<timestamp>/` に書き出され、古いものは自動で間引かれます (件数は `diff.retain` で調整可)。`--out` に渡すのはフォルダです (`--out report.html` と書くと `report.html/index.html` というフォルダができます)。書き出す画像の名前には、ファイル名を拡張子ごと使います (`illust.clip` と書き出した `illust.png` が同時に変わっても、`illust.clip-new.png`・`illust.png-new.png` のように別々のファイルになります)。

## レイヤー単位の操作

`add` / `restore` は `path:layer` 形式でレイヤーを指定できます。`layer` にはレイヤー名・フォルダパス (`人物/塗り/ドレス`)・ID (`clip:<id>` など) が使えます。同名レイヤーがあると名前だけでは指定できないので、`tmiki ls-layers` でパスと ID を確認してください (曖昧なときのエラーにも候補が出ます)。

```powershell
# レイヤー一覧 (ID・種別・表示・不透明度・合成モード・フォルダパス)
tmiki ls-layers illust.clip

# 特定レイヤーだけステージ
tmiki add illust.clip:line
tmiki add "illust.clip:人物/塗り/ドレス"

# 過去コミットからレイヤーを PNG で取り出す
tmiki restore --source HEAD~1 --output line_old.png illust.clip:line

# --output は既定でステージ (インデックス) から読む。add していない今の作業ファイルから書き出すなら --source WORKTREE
tmiki restore --source WORKTREE --output line_now.png illust.clip:line

# 過去コミットのレイヤーをネイティブファイルに一意なIDで安全に書き戻す
tmiki restore --source HEAD~1 --in-place illust.clip:line

# フォルダを指定すると中身を合成した PNG を書き出す
tmiki restore --output 人物.png illust.clip:人物

# レイヤーを指定しないと、ファイル全体を書き出す。出力先が元と同じ拡張子なら元のファイルそのもの、
# .png なら絵 (合成結果) を PNG で書く
tmiki restore --source HEAD~1 --output illust_old.clip illust.clip
tmiki restore --source HEAD~1 --output illust_old.png illust.clip
```

レイヤーを指定しない `--output` は、出力先の拡張子が元のファイルと同じ (`.jpg` と `.jpeg` は同じ扱い)・拡張子なし・画像以外の拡張子 (`.bak` など) なら元のファイルをそのまま、`.png` なら合成結果の PNG を書きます。それ以外の画像形式 (`.jpg`・`.gif`・`.clip` など) への変換はしないので、拡張子と中身が食い違うファイルを作る代わりにエラーで止まります。

## コミットの修正・一括ステージ

```powershell
tmiki add -A                 # 追加・変更・削除をまとめてステージ
tmiki add -u                 # 追跡済みファイルの変更・削除だけステージ
tmiki commit -a -m "進捗"   # 変更済みの追跡ファイルを自動でステージしてコミット
tmiki commit --amend        # 直前のコミットを置き換える (親は維持)
tmiki commit --allow-empty -m "checkpoint"
```

## ブランチ・マージ

```powershell
tmiki branch feature-eye         # 作成
tmiki branch                     # 一覧
tmiki branch -a -v               # remote-tracking branch とコミット要約も表示
tmiki branch --merged main       # main にマージ済みのブランチ
tmiki branch --contains v1.0     # 指定コミット/タグを含むブランチ
tmiki branch -d feature-eye      # マージ済みのブランチを削除
tmiki branch -D feature-eye      # 強制削除

tmiki switch feature-eye         # 切り替え
tmiki switch -c feature-hair     # ブランチを作って切り替え
tmiki switch main --discard      # 作業ツリーを捨ててでも切り替える

tmiki merge feature-eye -m "merge feature-eye"
```

同じファイルでも、別々のレイヤーの変更は自動でマージされます。片側だけのレイヤー追加・削除・リネーム・不透明度/合成モード/表示の変更も、もう片側がそのレイヤーに触れていなければ自動で取り込みます (両側がレイヤー構成を変えた場合は衝突になります)。

衝突すると、衝突しなかったレイヤーは作業ファイルに反映済みで、衝突したレイヤーだけが自分側 (OURS) のまま残ります。`tmiki merge` と `tmiki status` が衝突したレイヤーと理由を表示し、両側の PNG が `.tmi/conflicts/` に書き出されます。

```powershell
tmiki restore --theirs illust.clip:線画   # そのレイヤーだけ相手側を採用
tmiki restore --ours illust.clip          # ファイル全体を自分側に
tmiki restore --source MERGE_HEAD --output theirs.png illust.clip:線画
tmiki add illust.clip                      # 解消したら add → commit
tmiki commit -m "merge feature-eye"
tmiki merge --abort                        # マージをやめて HEAD に戻す
```

衝突ファイルを何も直さずに `add` すると、相手側の変更が捨てられる旨の警告が出ます (`tmiki restore --ours` / `--theirs` で選んだあとの `add` では出ません)。GIF・BMP・TIFF・WebP・PSD のようにレイヤーが無く、tmi が書き戻せない形式は、両側で変えると必ず衝突し、「`--ours` か `--theirs` でどちらかを選ぶ」と案内されます。

PNG / JPEG のような1枚絵は、両側の描き込みが離れていれば自動でマージされます。ただし JPEG は保存し直すたびに画像全体の画素が少しずつ変わるため、両側で編集すると描いた場所が離れていても衝突します (`--ours` / `--theirs` でどちらかを選びます)。ブランチで並行して描くファイルは PNG で保存することを勧めます。

マージや `restore` で tmi が PNG / JPEG を書き直すときは、元のファイル (マージでは自分側) の解像度 (dpi)・カラープロファイル (ICC)・EXIF とカラーモード (アルファのない画像は RGB のまま) を引き継ぎます。JPEG は元の量子化テーブルで保存し直します。

## タグ

```powershell
tmiki tag v1.0                        # 軽量タグ
tmiki tag -a v1.0 -m "first release"  # 注釈付きタグ
tmiki tag                             # 一覧
tmiki tag -l "v*" -n1                 # パターン一致 + 注釈1行
tmiki tag --points-at HEAD            # 指定コミットを指すタグ
tmiki tag -d v1.0                     # 削除
tmiki push origin v1.0                # タグを共有する (まとめては tmiki push --tags)
```

ブランチ名・タグ名・リモート名は `git check-ref-format` と同じ規則で検査します: `HEAD`、`-` で始まる名前、空白・制御文字・`~ ^ : ? * [ \`、`..`、`@{` を含む名前、`.` で始まる、または `.lock` で終わる構成要素、`.` や `/` で終わる名前は作れません (たとえば `main~1` というブランチを作れると `main~1` の意味が曖昧になるため)。日本語の名前 (`入稿版`、`feat/線画`) は使えます。

タグは動かしません。`tmiki push origin <tag>` / `tmiki push --tags` は、リモートに同じ名前で別のコミットを指すタグがあると断ります (`--force` で置き換えられるのはローカルパスのリモートだけです)。clone と fetch は、リモートのタグのうち手元に無いものを取り込みます (手元に同名のタグがあれば、そのまま残します)。

## リモート

ローカルパス (bare リポジトリも可) と、HTTPS でホストする tmihub (別リポジトリのサーバー) の両方に対応しています。clone / fetch は現状すべてのオブジェクトを取得します (大きな blob を必要になってから取る遅延 fetch は未実装・予定)。

```powershell
# ローカル / bare リポジトリ
tmiki remote add origin path\to\bare-repo
tmiki remote                       # 一覧
tmiki remote -v                    # fetch/push URL 付きで一覧
tmiki remote remove origin
tmiki fetch origin
tmiki fetch --all
tmiki push origin main
tmiki push -f origin main          # 非 fast-forward を強制 (ローカルリモートのみ)
tmiki push --force-with-lease origin main
tmiki pull origin main
tmiki clone path\to\source new-dir
```

### HTTPS リモート (tmihub)

tmihub の URL はリポジトリの API ルート `…/api/<user>/<repo>` を指します。push と非公開リポジトリの読み取りにはパーソナルアクセストークン (PAT) が必要です (公開リポジトリの clone / pull はトークン不要)。

```powershell
# トークンを設定 (どちらか): tmihub の「設定 → アクセストークン」で発行
tmiki config --global tmihub.token <PAT>
$env:TMIHUB_TOKEN = "<PAT>"          # または環境変数

# リモート登録 → push
tmiki remote add origin https://example.com/api/<user>/<repo>
tmiki push origin main

# clone / pull も同様
tmiki clone https://example.com/api/<user>/<repo>
tmiki pull origin main
```

トークンは環境変数 `TMIHUB_TOKEN` → グローバル設定 `tmihub.token` の順に解決されます。HTTPS 越しの強制 push (非 fast-forward) は tmihub 側が拒否します (409) — 先に `tmiki pull` してマージしてください。

## 復旧 (reflog / gc)

```powershell
tmiki recover                          # HEAD の移動履歴を表示
tmiki recover HEAD@{2}                 # 指定地点へ戻す
tmiki recover --as rescue HEAD@{2}     # 指定地点でブランチを作る

tmiki gc                               # 90日より古い reflog エントリ経由のオブジェクトを掃除
tmiki gc --prune now                   # 即時に到達不能オブジェクトを削除

tmiki recover --files                  # 上書き前に自動退避したファイルの一覧
tmiki recover --file 0                 # 退避したファイルを元の場所に戻す (--output で別の場所へ)
```

`restore` / `switch --discard` / `reset --hard` / `merge --abort` などで、コミットされていない作業ファイルを上書き・削除するときは、元のファイルを自動で退避します。`gc` は diff レポート・キャッシュ・一時ファイル (`.tmi/diff` `.tmi/cache` `.tmi/tmp`) も掃除し、退避ファイルは reflog と同じ期間だけ残します。

作業ファイルを書き換えたあとは、描画アプリで開いたままなら開き直してください (古い内容のまま上書き保存すると戻ってしまいます)。

`tmiki reflog` は `tmiki recover` のエイリアスです。

## フック・プラグイン

`.tmi/hooks/<name>` に置いた実行ファイル/スクリプトが、`pre-commit` / `post-commit` / `pre-push` / `post-merge` / `pre-switch` / `post-switch` のタイミングで呼ばれます。

```powershell
tmiki hook list                  # 登録されているフックを表示
tmiki hook run post-commit       # 手動で発火
```

時間/操作トリガの自動コミットなどはコア外で、プラグイン (`tmi.plugins`) として後付けする想定です。

## 形式プラグイン

tmi が組み込みで読まない形式は、WebAssembly の component (`tmi:format` の reader。PLUGINS.md) で読ませられます。component はリポジトリの `.tmiplugins/<id>.wasm` に置いて普通のファイルとして commit し、どのファイルをどのプラグインで読むかは `.tmiattributes` に書きます。clone した人も同じ版で取り込みます。プラグインを動かすのは `tmi` の実行ファイルだけです (`python -m tmi` はプラグイン形式のファイルを断ります)。

```powershell
tmiki plugin add ora.wasm          # .tmiplugins/ora.wasm に置き、.tmiattributes に「*.ora format=plugin:ora」を足して両方ステージ
tmiki plugin add ora.wasm '*.ora' 'scans/*.ora'   # 対応づけるパターンを指定 (既定はプラグインが名乗る拡張子)
tmiki add art.ora                  # 以後は普通に add / status / diff / merge
tmiki plugin list                  # 組み込みとリポジトリのプラグイン (版・パターン・sha256・信頼・キャッシュ)
tmiki plugin prepare               # 先にコンパイルしておく (clone の最後にも自動で行う)
tmiki plugin reimport ora          # プラグインを上げたあと、そのプラグインで読むファイルを全部取り込み直す
tmiki add --renormalize [path...]  # .tmiattributes を変えたあと、変わっていないファイルも取り込み直す
tmiki plugin trust ora             # 書き戻し (マージ・restore --in-place など) を許す (リポジトリと component ごとに一度)
tmiki plugin remove ora            # プラグインをやめる (下の「プラグインをやめるとき」)
```

書き戻しのできるプラグイン (`tmi:format` の writer を持ち、`can-write` を名乗るもの) なら、レイヤー単位の操作も組み込みの形式と同じように使えます。

```powershell
tmiki restore --source HEAD~1 --in-place art.ora:影   # 1 レイヤーを作業ファイルに書き戻す
tmiki restore --theirs art.ora:影                    # マージの衝突で、相手のレイヤーを取る
tmiki add art.ora:影                                 # 1 レイヤーだけをステージする (作業ファイルはそのまま)
tmiki merge side                                     # 両側で別のレイヤーを変えていれば、そのままマージされる
```

`.tmiattributes` の書き方 (パターンは `.tmiignore` と同じ。大文字・小文字はどの OS でも区別しない。属性ごとに最後に当たった行が勝つ):

```
*.ora          format=plugin:ora
*.psd          format=binary        # 不透明なファイルとして記録する
*.ora          on-error=binary      # プラグインが読めないファイルを不透明として残す (既定は refuse: add を止める)
```

- プラグインはファイルシステム・ネットワーク・時刻に触れないサンドボックスで動きます。既定 (`plugin.trust=notice`) では、リポジトリと component の組ごとに最初の一度だけ「this repository uses format plugin ora 0.2.0 (sha256 ...). It runs in a sandbox: ...」と知らせ、止めはしません。`tmiki config --global plugin.trust prompt` で最初に聞く (端末で `[y/N]` と聞き、答えるまで進捗の表示は止まります)、`deny` で使わない、`allow` で何も言わない (書き戻しも許す) にできます。記録は `trust.json` (ユーザーの config と同じフォルダ) に残ります。
- プラグインでファイルを書き戻す前 (マージの書き戻し・`restore --in-place`・`restore --ours/--theirs <file>:<layer>`・`add <file>:<layer>`) には、リポジトリと component ごとに一度、許可を求めます。端末でなければ何も変えずに止まり、`tmiki plugin trust <id>` (か `plugin.trust=allow`) で許すよう案内します。
- 書き戻した結果は、同じプラグインで読み直して確かめます: 書き換えたレイヤーが新しい画素になっていて、ほかのレイヤーの画素とプロパティ・キャンバスが元のままであること。通らなければ作業ファイルは置き換えません (マージならそのファイルは衝突になります)。未保存の変更がある作業ファイルは、置き換える前に退避します (`tmiki recover --files`)。
- 書き戻しのできないプラグインで、両側が変えたファイルのマージは、ファイル全体の衝突になります。どちらがどのレイヤーを変えたかを示すので、`tmiki restore --ours <file>` か `--theirs <file>` で片方を選び、もう片方の変更は描画アプリで入れ直してください (両方の版とレイヤーの PNG は `.tmi/conflicts/<file>/` にあります)。
- 合成画像 (サムネイル・diff の画像・`restore --output x.png`) は、プラグインが返したものがあればそれを使います。ORA なら `mergedimage.png` をそのまま使い、レイヤーから合成し直しません (無いときだけ tmi が合成します)。`mergedimage.png` を更新しないツールで編集すると、レイヤーの差分は出てもサムネイルは変わりません。
- プラグインが無い・読めない・信頼されていないとき: `add` / `commit -a` / `mv` / `stash push` は止まります (不透明として黙って記録はしません)。`status` は「(layer details need plugin 'ora')」、作業ツリーとの `diff` はファイル単位の比較になり、コミット同士の `diff`・`log`・`show`・`switch`・`checkout`・ファイル全体の `restore` はそのまま使えます。`ls-layers` は版を付ければ記録済みの木を出します (`tmiki ls-layers art.ora HEAD`。版なしだと、作業ツリーのファイルが記録と違うときに取り込もうとして止まります)。両側で変えたファイルのマージは衝突になり、`--ours` / `--theirs` で選びます。
- プラグインをやめるとき: `tmiki plugin remove ora` は `.tmiplugins/ora.wasm` を消し、`.tmiattributes` から `format=plugin:ora` を外し、そのプラグインで読んでいた追跡中のファイルを作業ツリーの中身から取り込み直します (ORA なら不透明なファイルになります)。どれもステージされるので、`tmiki commit` で確定します。前の commit の記録はそのままです (`tmiki ls-layers art.ora HEAD~1`)。`.tmiplugins/ora.wasm` だけを `tmiki rm` すると `.tmiattributes` に対応づけが残り、以後そのファイルを add できなくなるので、警告が出ます。
- `plugin.fuel` (1 回の呼び出しの命令数)・`plugin.timeout` (秒)・`plugin.maxMemory` (バイト。`512m` のような書き方も可) で上限を変えられます。コンパイル結果は `%LOCALAPPDATA%\tmi\plugin-cache` (`TMI_PLUGIN_CACHE` で変更) にキャッシュします。
- テスト用の ORA プラグインは `python tools\build_test_plugins.py --ora` で作れます (書き戻しのできる 0.3.0。`--ora-version 0.1.0 --read-only` で読むだけの版。`rust\plugins`。`rustup target add wasm32-wasip2` が要ります)。

### プラグインを作る

`tmi` の実行ファイルに、プラグインを書く人のための道具が入っています (PLUGINS.md 7.1。リポジトリの外で使います)。プラグインは Rust で書きます (PLUGINS.md 11 節 Q14。速さと軽さのため。AI に書かせる前提なので、作者の言語の好みより実行時の速さを取りました)。インターフェースの全部は `PLUGIN_SPEC.md` (`rust\crates\tmi-plugin-tools\PLUGIN_SPEC.md`。`plugin new` がプラグインのディレクトリにも書き出します) にあり、それだけを AI に渡して書かせられるように書いてあります。

```powershell
tmiki plugin new kra --writer               # kra\ に Rust の雛形 (作り物の「tmi sample」形式を読む)
cd kra
tmiki plugin doctor                         # cargo と wasm32-wasip2 が入っているか
tmiki plugin build                          # component (target\tmi-plugin\kra.wasm) と開発用プラグインを作る
tmiki plugin test                           # PLUGINS.md 7.1 の検査を全部 (失敗を全部挙げて 1 で終わる)
tmiki plugin test --native                  # 同じコードの開発用プラグインが component と同じ答えを返すか
tmiki plugin test --update                  # tests\corpus\*.expect.json を書く (以後は記録の変化を見張る)
tmiki plugin test --fuzz 10000              # 壊したファイルでも tmi が落ちないか
tmiki plugin test --against old.wasm        # 前の版と記録がどう違うか
tmiki plugin install target\tmi-plugin\kra.wasm   # %USERPROFILE%\.tmi\plugins\ に置く (TMI_PLUGIN_HOME で変更)
tmiki plugin add kra                        # (リポジトリの中で) 置いた component を使う。kra@0.1.0 で版を指定
```

- 雛形は SDK `tmi-plugin-sdk` (`rust\plugins\sdk`) の上に書いてあり、同じコードが component にも、標準入出力で tmi と話す普通のプログラム (開発用プラグイン) にもなります。プラグインのディレクトリはリポジトリの外にあるので、`plugin new` は SDK のソースの写しを `sdk\` に書き出します。
- `--lang` は `rust` だけです (省略可)。`--lang python` などは「tmi のプラグインは Rust で書く」と言って断ります。
- 開発用プラグインは、component を作り直さずに本物のコマンドで試せます: `tmiki plugin dev target\debug\kra-dev -- add a.kra`。`--` が無ければ info と `.tmiattributes` の対応を見せます。環境変数 `TMI_PLUGIN_DEV` (プログラム、またはその JSON の配列) でも指せます。開発用プラグインの記録は `component_sha256 = "dev"` を持ち、`commit`・`stash` が断ります。サンドボックスの外で動く自分のプログラムなので、信頼と書き戻しの同意は聞きません。

## 設定

```powershell
tmiki config user.name "Hogehoge"           # リポジトリ単位
tmiki config --global user.email me@gmail.com  # 全リポジトリ共通
tmiki config --list                       # 有効値を一覧
tmiki config --unset diff.retain
```

## コマンド早見表

| コマンド | 用途 |
| --- | --- |
| `init [path] [--bare]` | リポジトリ作成 |
| `status [-s] [-b] [--porcelain]` | 作業ツリーの状態 |
| `add [-A\|-u] [--renormalize] <spec>...` | ステージ (`path` または `path:layer`。`--renormalize` は変わっていなくても取り込み直す) |
| `commit -m <msg> [-a] [--amend] [--allow-empty]` | コミット (合成サムネ自動添付) |
| `log [--graph] [--oneline] [--decorate] [--all] [-n N] [--stat\|--name-only\|--name-status] [rev]` | 履歴表示 |
| `show [--stat\|--name-only\|--name-status] [--thumbnail [-o FILE]] [rev\|rev:path]` | コミット / ファイルの表示、サムネの取り出し |
| `diff [--staged] [rev [rev]] [--side-by-side\|--overlay\|--per-layer\|--html] [--name-only\|--name-status\|--stat] [--out DIR] [--no-open] [[--] path...]` | ビジュアル diff / 要約 |
| `ls-layers <path> [rev]` | レイヤー一覧 (ID・フォルダパス) |
| `branch [-a\|-r] [-v] [--merged REV] [--contains REV] [-d\|-D] [name [start]]` | ブランチ作成 / 一覧 / 削除 |
| `switch [-c] <name> [start] [--discard]` | ブランチ切替 |
| `checkout [-b NAME] [--discard] <branch\|rev\|path>` | 切替 / 復元 (Git 互換) |
| `restore [-S\|--staged] [--source REV] [--in-place\|--output FILE] [--ours\|--theirs] <spec>...` | 復元 (`--staged` でステージ取り消し、`--ours/--theirs` で衝突解消) |
| `reset [--soft\|--mixed\|--hard] [rev] [-- path...]` | HEAD / インデックスを戻す |
| `rm [-f] [--cached] <path>...` / `mv <src> <dst>` | 削除 / 移動 |
| `stash [push\|list\|apply\|pop\|drop\|clear]` | 作業の一時退避 (サブコマンドを省くと `push`) |
| `tag [-l] [-nN] [--points-at REV] [-a] [-m MSG] [-d] [name [rev]]` | タグ作成 / 一覧 / 削除 |
| `remote [-v] [add\|remove\|rm] ...` | リモート管理 |
| `fetch [--all] [remote]` | リモートから取得 |
| `push [-f\|--force-with-lease] [--tags] [remote] [branch\|tag]` | プッシュ (`--tags` は全タグ) |
| `pull [remote] [branch]` | フェッチ + マージ |
| `clone <url> [dir]` | クローン |
| `merge <rev> [-m MSG]` / `merge --abort` | マージ / 中止 |
| `hook [list\|run <name>]` | フックの確認・実行 |
| `plugin [add <file.wasm\|id[@version]> [pattern...] [--force]\|list\|prepare\|reimport <id>\|trust <id> [--revoke]\|remove <id>]` | 形式プラグイン (`tmi` の実行ファイルだけ) |
| `plugin [new <id> [--lang rust] [--writer]\|build [dir]\|test [dir\|x.wasm] [--native] [--native-command cmd] [--update] [--fuzz N] [--against old.wasm] [--dump]\|doctor [dir]\|install <x.wasm>\|dev <program> [args...] [-- <command>...]]` | プラグインを書く人の道具 (リポジトリの外。`dev` は中でも) |
| `gc [--prune <spec>]` | 到達不能オブジェクトの掃除 |
| `recover` / `reflog` `[--as branch] [rev]` / `recover --files` / `recover --file N` | HEAD 履歴の表示 / 復旧、自動退避ファイルの復元 |
| `config [--global] [--list\|--unset] [key [value]]` | 設定 |
| `--version` | tmi と tmi_accel の版 (バグ報告用) |

MVP の範囲は Git のメインライン (`init` / `clone` / `status` / `add` / `commit` / `log` / `diff` / `branch` / `switch` / `merge` / `restore` / `tag` / `remote` / `fetch` / `pull` / `push`) です。
