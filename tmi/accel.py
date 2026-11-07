"""tmi's native core: the ``tmi_accel`` extension module (``rust/``).

tmi needs it: the CLIP renderer, the layer blob (TMIL) codec, PNG encoding,
content-defined chunking, the CLIP / MDP2 tile decoders and the blend
kernels live there, with no Python fallback (RUST_MIGRATION.md 1e; the
Python renderer is kept apart, as the reference package ``tmiref``).

The module is loaded on first use (``native()``), not when this one is
imported, so commands that never touch pixels start without it. A missing
or outdated build raises ``AccelUnavailableError``, which is not an
``Exception``: no fallback that catches ``Exception`` may swallow it and
carry on without pixels (``tmi.cli`` reports it as an error).
"""
from __future__ import annotations

# What this tmi relies on (tmi_accel's FEATURES). An older build lacks some.
REQUIRED_FEATURES = (
    "concurrent-calls", "tmil-v1", "render-clip-v2", "offscreen-v2", "image-v1", "mdp2-v1", "clip-io-v1",
    "image-sparse-v1", "image-import-v1",
)

INSTALL_HINT = (
    "install it with `pip install ./rust/tmi-accel` from a tmiki checkout, which needs a Rust toolchain "
    "(https://rustup.rs) and maturin, or from the tmi-accel wheel of a tmi release (not from PyPI: "
    "tmi is not published there)"
)


class AccelUnavailableError(BaseException):
    """tmi_accel is not installed, or too old for this tmi.

    A BaseException (like SystemExit) so that it reaches the user instead of
    a fallback that catches Exception, e.g. tracking a drawing as an opaque
    file."""


_native = None


def native():
    """The tmi_accel module, checked once; raises AccelUnavailableError."""
    global _native
    if _native is None:
        _native = _load()
    return _native


def _load():
    try:
        import tmi_accel
    except ImportError as exc:
        raise AccelUnavailableError(
            f"tmi needs its native module tmi_accel, which cannot be imported ({exc}); {INSTALL_HINT}"
        ) from None
    features = set(getattr(tmi_accel, "FEATURES", ()))
    missing = [name for name in REQUIRED_FEATURES if name not in features]
    if missing:
        raise AccelUnavailableError(
            f"the installed tmi_accel is older than this tmi (it lacks {', '.join(missing)}); "
            f"reinstall it: {INSTALL_HINT}"
        )
    return tmi_accel


def has_feature(name: str) -> bool:
    """Whether tmi_accel declares ``name`` in its FEATURES."""
    return name in getattr(native(), "FEATURES", ())
