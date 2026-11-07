"""How large an image (canvas or layer) tmi agrees to decode.

Pillow ships a decompression-bomb guard (``Image.MAX_IMAGE_PIXELS``, ~89 Mpx)
that warns above the limit and fails above twice of it. Print-size canvases
(A1/A0 at 350 dpi) cross it routinely, so tmi replaces it with its own limit:

- ``TMI_MAX_IMAGE_PIXELS`` (environment) wins, then ``core.maxImagePixels``
  from the repo config, then the user config; ``0`` means no limit.
- Otherwise the default is derived from the machine's physical memory:
  ``max(FLOOR, total_ram / 2 / bytes_per_pixel)``. ``FLOOR`` is the size at
  which Pillow used to fail outright, so nothing that worked before stops.
  ``bytes_per_pixel`` depends on what is decoded: a layered drawing (CLIP /
  MDP2, checked by ``check_pixels`` on its canvas) is composited as a whole
  and needs far more memory per pixel than a single raster image (PNG / JPEG,
  checked by Pillow), so the two kinds get separate defaults. An explicit
  setting applies to both.

``apply_to_pil()`` installs the raster limit into Pillow; ``tmi.formats``
calls it at import and ``tmi.image_ops`` when it first uses Pillow. Images
tmi_accel decodes without Pillow (``tmi.raster``, layer blobs) are checked
by ``check_raster_pixels`` against the same limit. An image over the limit
raises :class:`CanvasTooLargeError` (a tmi message, never a Pillow
``DecompressionBombWarning``).

Stdlib only: importing this must stay cheap for status/log.
"""

from __future__ import annotations

import os
import sys
import warnings
from dataclasses import dataclass
from pathlib import Path

CONFIG_KEY = "core.maxImagePixels"
ENV_VAR = "TMI_MAX_IMAGE_PIXELS"

# Pillow's hard-error threshold (2 x its default MAX_IMAGE_PIXELS).
FLOOR = 178_956_970

# Peak working set per canvas pixel, measured with `import_file` (what
# `tmiki add` / `status` run). CLIP tama.clip (11.8 Mpx) peaks at ~51 B/px with
# tmi_accel and ~98 B/px without (every decode worker holds canvas-sized RGBA
# and float32 blend buffers); lolita page0001's full composite at ~82 B/px.
# A single 182 Mpx raster PNG peaks at ~16-17 B/px (15.6 B/px on Linux). The
# worst case of each kind is used so the default stays within half of
# physical memory.
LAYERED_BYTES_PER_PIXEL = 100
RASTER_BYTES_PER_PIXEL = 20
LAYERED = "layered"
RASTER = "raster"
_BYTES_PER_PIXEL = {LAYERED: LAYERED_BYTES_PER_PIXEL, RASTER: RASTER_BYTES_PER_PIXEL}


class CanvasTooLargeError(RuntimeError):
    def __init__(self, pixels: int, limit: "PixelLimit", subject: str | None = None) -> None:
        self.pixels = pixels
        self.limit = limit
        self.subject = subject
        super().__init__(_too_large_message(pixels, limit, subject))


@dataclass(frozen=True)
class PixelLimit:
    value: int  # 0 = no limit
    source: str  # "env", "repo config", "user config" or "default"
    ram_bytes: int | None = None
    kind: str = LAYERED  # what a "default" value was computed for

    def origin(self) -> str:
        if self.source == "env":
            text = f"from {ENV_VAR}"
        elif self.source in ("repo config", "user config"):
            text = f"from the {self.source}"
        elif self.source != "default":
            text = f"from {self.source}"
        elif self.ram_bytes:
            text = f"default, from {_gb(self.ram_bytes)} of memory"
        else:
            text = "default; this machine's memory could not be detected"
        return f"no limit, {text}" if self.value == 0 else text


_repo_dir: Path | None = None
_applied: PixelLimit | None = None
_ram_cache: list[int | None] = []


def set_repo_dir(repo_dir: Path | None) -> None:
    """Use ``repo_dir``'s config from now on (re-applied if Pillow is loaded)."""
    global _repo_dir, _applied
    _repo_dir = repo_dir
    _applied = None
    if "PIL.Image" in sys.modules:
        apply_to_pil()


def total_ram_bytes() -> int | None:
    """Installed physical memory, or None when it cannot be detected."""
    if not _ram_cache:
        try:
            _ram_cache.append(_detect_total_ram())
        except Exception:
            _ram_cache.append(None)
    return _ram_cache[0]


def _detect_total_ram() -> int | None:
    import ctypes

    if sys.platform == "win32":
        class MEMORYSTATUSEX(ctypes.Structure):
            _fields_ = [
                ("dwLength", ctypes.c_ulong),
                ("dwMemoryLoad", ctypes.c_ulong),
                ("ullTotalPhys", ctypes.c_ulonglong),
                ("ullAvailPhys", ctypes.c_ulonglong),
                ("ullTotalPageFile", ctypes.c_ulonglong),
                ("ullAvailPageFile", ctypes.c_ulonglong),
                ("ullTotalVirtual", ctypes.c_ulonglong),
                ("ullAvailVirtual", ctypes.c_ulonglong),
                ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
            ]

        status = MEMORYSTATUSEX()
        status.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
        if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):  # type: ignore[attr-defined]
            return None
        return int(status.ullTotalPhys) or None
    if sys.platform == "darwin":
        libc = ctypes.CDLL(None)
        value = ctypes.c_uint64(0)
        size = ctypes.c_size_t(ctypes.sizeof(value))
        if libc.sysctlbyname(b"hw.memsize", ctypes.byref(value), ctypes.byref(size), None, ctypes.c_size_t(0)) != 0:
            return None
        return int(value.value) or None
    pages = os.sysconf("SC_PHYS_PAGES")
    page_size = os.sysconf("SC_PAGE_SIZE")
    if pages <= 0 or page_size <= 0:
        return None
    return int(pages) * int(page_size)


def default_limit(ram_bytes: int | None, kind: str = LAYERED) -> int:
    if not ram_bytes:
        return FLOOR
    return max(FLOOR, (ram_bytes // 2) // _BYTES_PER_PIXEL[kind])


def parse_value(raw: object, origin: str | None = None) -> int:
    text = str(raw).strip().replace("_", "")
    try:
        value = int(text)
    except ValueError:
        value = -1
    if value < 0:
        where = f" (in {origin})" if origin else ""
        raise RuntimeError(f"{CONFIG_KEY} must be a whole number of pixels, 0 for no limit; got {raw!r}{where}")
    return value


def resolve(repo_dir: Path | None = None, kind: str = LAYERED) -> PixelLimit:
    """The effective limit for ``kind`` (LAYERED canvases or RASTER images):
    env > repo config > user config > RAM default."""
    from . import config as _config

    env = os.environ.get(ENV_VAR, "").strip()
    if env:
        return PixelLimit(parse_value(env, ENV_VAR), "env")
    repo_dir = repo_dir if repo_dir is not None else _repo_dir
    if repo_dir is not None:
        raw = _config.get_value(_config.read_config_file(repo_dir / "config"), CONFIG_KEY)
        if raw is not None:
            return PixelLimit(parse_value(raw, "repo config"), "repo config")
    raw = _config.get_value(_config.read_user_config(), CONFIG_KEY)
    if raw is not None:
        return PixelLimit(parse_value(raw, "user config"), "user config")
    ram = total_ram_bytes()
    return PixelLimit(default_limit(ram, kind), "default", ram, kind)


def describe(repo_dir: Path | None = None) -> str:
    """One line for ``tmiki config core.maxImagePixels``: the effective limit
    and where it comes from (both defaults when nothing is set)."""
    layered = resolve(repo_dir, LAYERED)
    if layered.source != "default":
        return f"{layered.value} ({layered.origin()})"
    raster = resolve(repo_dir, RASTER)
    return (
        f"{layered.value} for CLIP/MDP canvases, {raster.value} for other images "
        f"({layered.origin()})"
    )


def current() -> PixelLimit:
    """The raster limit installed into Pillow."""
    return _applied if _applied is not None else resolve(kind=RASTER)


def check_pixels(width: int | None, height: int | None, subject: str | None = None) -> None:
    """Refuse a layered canvas (CLIP / MDP2) tmi would have to composite as a
    whole."""
    if not width or not height:
        return
    limit = resolve(kind=LAYERED)
    pixels = int(width) * int(height)
    if limit.value and pixels > limit.value:
        raise CanvasTooLargeError(pixels, limit, subject)


def check_raster_pixels(size: tuple[int, int]) -> None:
    """The check Pillow makes when it opens an image (``_pil_check``), for
    images tmi decodes without Pillow (``tmi.raster``): over the raster
    limit is a CanvasTooLargeError with tmi's message."""
    if "PIL.Image" in sys.modules:
        # Pillow is loaded: its installed limit (apply_to_pil) is the one in
        # force, as for the images it opens.
        from PIL import Image

        if getattr(Image, "_decompression_bomb_check", None) is _pil_check:
            _pil_check(size)
            return
    limit = resolve(kind=RASTER)
    pixels = max(1, size[0]) * max(1, size[1])
    if limit.value and pixels > limit.value:
        raise CanvasTooLargeError(pixels, limit)


def apply_to_pil() -> PixelLimit:
    """Install the effective limit into Pillow (idempotent per setting)."""
    global _applied
    from PIL import Image

    limit = resolve(kind=RASTER)
    Image.MAX_IMAGE_PIXELS = limit.value or None
    # Never show Pillow's "could be decompression bomb DOS attack" warning:
    # over the limit is reported by the check below instead.
    warnings.filterwarnings("ignore", category=Image.DecompressionBombWarning)
    check = getattr(Image, "_decompression_bomb_check", None)
    if check is not None and not getattr(check, "_tmi", False):
        Image._decompression_bomb_check = _pil_check  # type: ignore[attr-defined]
    _applied = limit
    return limit


def _pil_check(size: tuple[int, int]) -> None:
    # Replaces PIL.Image._decompression_bomb_check (Pillow calls it on open and
    # from several plugins): fail at the limit itself, with tmi's message, and
    # never warn.
    from PIL import Image

    maximum = Image.MAX_IMAGE_PIXELS
    if maximum is None:
        return
    pixels = max(1, size[0]) * max(1, size[1])
    if pixels > maximum:
        limit = current()
        if limit.value != maximum:
            limit = PixelLimit(int(maximum), "PIL.Image.MAX_IMAGE_PIXELS")
        raise CanvasTooLargeError(pixels, limit)


_pil_check._tmi = True  # type: ignore[attr-defined]


def _gb(n: int) -> str:
    return f"{n / 2**30:.1f} GB"


def _suggested_value(pixels: int) -> int:
    step = 1_000_000
    return -(-pixels // step) * step


def _too_large_message(pixels: int, limit: PixelLimit, subject: str | None) -> str:
    what = f"{subject}: canvas too large" if subject else "canvas too large"
    suggestion = _suggested_value(pixels)
    if limit.source == "env":
        return (
            f"{what}: {pixels} pixels (limit {limit.value}, set by {ENV_VAR}). "
            f"Raise it with {ENV_VAR}={suggestion}  (0 = no limit)"
        )
    if limit.source in ("repo config", "user config"):
        scope = "" if limit.source == "repo config" else "--global "
        return (
            f"{what}: {pixels} pixels (limit {limit.value}, set by {CONFIG_KEY} in the {limit.source}). "
            f"Raise it with: tmiki config {scope}{CONFIG_KEY} {suggestion}  (0 = no limit)"
        )
    if limit.source == "default":
        basis = (
            f"based on {_gb(limit.ram_bytes)} of memory" if limit.ram_bytes else "this machine's memory could not be detected"
        )
        scope = "CLIP/MDP canvases" if limit.kind == LAYERED else "images"
        return (
            f"{what}: {pixels} pixels (limit {limit.value} for {scope}, {basis}); this machine may not have enough "
            f"memory to process it. If it does, raise it with: tmiki config {CONFIG_KEY} {suggestion}  (0 = no limit)"
        )
    return f"{what}: {pixels} pixels (limit {limit.value}, set by {limit.source})"
