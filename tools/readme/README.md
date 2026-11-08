# README の画像の作り方

README の画像は、ここにあるスクリプトで作った見本の絵（積み木の家・4 レイヤーの `.mdp`）を tmiki で実際に操作して撮ったものです。外部の作品は使っていません。

- `make_demo_mdp.py`：見本の `.mdp` を描く（Pillow）。`--rough` / `--green` / `--flag` で版を切り替える
- `make_logo.py`：`docs/brand/` のロゴ（ライト・ダーク・マークだけ）を書く
- `term_html.py`：記録した端末の出力を、手を加えずに端末風の HTML にする
- `shot_report.js` / `shot_el.js`：Playwright で `diff --html` の報告と端末の HTML を撮る（`PW_DIR` に playwright を入れたフォルダ）

手順：

```powershell
python make_demo_mdp.py D:\tsumiki\tsumiki.mdp --rough
tmiki init; tmiki add tsumiki.mdp; tmiki commit -m "ラフ"
python make_demo_mdp.py D:\tsumiki\tsumiki.mdp;          tmiki commit -a -m "塗りと影"
tmiki switch -c flag
python make_demo_mdp.py D:\tsumiki\tsumiki.mdp --flag;   tmiki commit -a -m "旗を立てる"
tmiki switch main
python make_demo_mdp.py D:\tsumiki\tsumiki.mdp --green;  tmiki commit -a -m "屋根を緑に"
tmiki merge flag -m "旗の案を取り込む"
tmiki diff --html --no-open <「塗りと影」のコミット> HEAD
```
