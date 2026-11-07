"""tmiref: tmi's Python reference implementations. Not part of tmi.

tmi renders and encodes through its native module ``tmi_accel``
(RUST_MIGRATION.md 1e). The Python code those Rust parts were ported from,
and are tested against byte for byte, lives here:

  clip          the CLIP compositor (render_layered_composite_rgba,
                render_layered_regions, render_folder, render_full), with the
                Python tile decoder and the numpy blend / correction kernels
  clip_style    表現色 / レイヤーカラー, image materials, フチ
  clip_tone     トーン
  clip_gradient gradient fills
  clip_vector   vector strokes
  clip_frame    comic frames
  mdp2          the MDP2 reader (ElementTree), layer decode, compositor and
                layer write-back (Pillow + zlib)
  render_trace  its TMI_RENDER_TRACE records
  layer_blob    the TMIL layer blob codec (numpy + zstandard + Pillow)
  chunking      content-defined chunking cut points (numpy)
  png           tmi's numpy PNG band encoder before 1e (test input)

Use it to refine format and rendering formulas against CSP's own output (the
probe workflow of research/clip-format.md) and as the oracle of the parity
tests and tools (tests/test_render_parity.py, tests/test_rust_kernels.py,
tools/render_corpus.py --render python, tools/render_diff.py). It imports
tmi (the CLIP parser, the layer buffers, the warnings); tmi never imports
it, and it is not in tmi's wheel. Needs numpy, Pillow and zstandard.
"""
