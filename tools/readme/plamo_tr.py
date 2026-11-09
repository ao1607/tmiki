"""Translate English README segments to Japanese with PLaMo 3 Translate (Ollama).

    python plamo_tr.py segments.json out.json
    python plamo_tr.py --one "English text"

Inline code (`...`) and bold markers are swapped for placeholders before
translation and put back after, so PLaMo never sees commands.
"""
import json
import re
import sys
import urllib.request

MODEL = "hf.co/pfnet/plamo-3-translate-8b-GGUF:Q4_K_M"
T, M, K, V = "<|plamo:tag|>", "<|plamo:msg|>", "<|plamo:key|>", "<|plamo:val|>"
IX, IT = "<|plamo:index|>", "<|plamo:item|>"
CONTEXT = ("tmiki はイラストのファイル（CLIP STUDIO PAINT の .clip、メディバンペイントと FireAlpaca の .mdp）を"
           "Git と同じ操作でバージョン管理するツール。その README の文章。読み手はイラストを描く人と開発者。")
GLOSSARY = [
    ("tmiki", "tmiki"), ("layer", "レイヤー"), ("layers", "レイヤー"), ("commit", "コミット"),
    ("branch", "ブランチ"), ("merge", "マージ"), ("diff", "diff"), ("line art", "線画"),
    ("colour", "塗り"), ("drawing app", "描画アプリ"), ("plugin", "プラグイン"), ("repository", "リポジトリ"),
    ("CLIP STUDIO PAINT", "CLIP STUDIO PAINT"), ("MediBang Paint", "メディバンペイント"), ("FireAlpaca", "FireAlpaca"),
    ("thumbnail", "サムネイル"), ("conflict", "衝突"), ("release", "リリース版"),
]


def protect(text):
    keep = []

    def sub(m):
        keep.append(m.group(0))
        return f"[[{len(keep) - 1}]]"

    text = re.sub(r"`[^`]+`|\*\*|<code>[^<]+</code>|\[([^\]]+)\]\([^)]+\)", sub, text)
    return text, keep


def restore(text, keep):
    for i, k in enumerate(keep):
        text = text.replace(f"[[{i}]]", k)
    return text


def translate(en, context=CONTEXT):
    src, keep = protect(en)
    p = f"{T}dataset{M}translation{T}context{M}{context}"
    words = [(a, b) for a, b in GLOSSARY if re.search(r"\b" + re.escape(a) + r"\b", src, re.I)]
    if words:
        p += f"{T}glossary" + "".join(f"{IX}{a}{IT}{b}" for a, b in words)
    p += f"{T}input{K}lang{V}en{M}{src}{T}output{K}lang{V}ja{K}style{V}polite{M}"
    body = json.dumps({"model": MODEL, "prompt": p, "raw": True, "stream": False,
                       "options": {"temperature": 0, "num_predict": 600,
                                   "stop": ["<|plamo:tag|>", "<|plamo:bos|>", "<|plamo:op|>"]}}).encode()
    req = urllib.request.Request("http://127.0.0.1:11434/api/generate", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as r:
        out = json.load(r)["response"].strip()
    missing = [k for i, k in enumerate(keep) if f"[[{i}]]" not in out]
    return restore(out, keep), missing


if __name__ == "__main__":
    if sys.argv[1] == "--one":
        print(translate(sys.argv[2]))
        sys.exit()
    segs = json.load(open(sys.argv[1], encoding="utf-8"))
    out = []
    for s in segs:
        ja, missing = translate(s["en"])
        out.append({**s, "plamo": ja, "missing": missing})
        print(f"[{s['id']}] {ja}" + (f"   (lost: {missing})" if missing else ""), flush=True)
    json.dump(out, open(sys.argv[2], "w", encoding="utf-8"), ensure_ascii=False, indent=1)
