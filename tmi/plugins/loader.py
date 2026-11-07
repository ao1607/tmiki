"""Discovery and one-time initialization of installed plugins."""

from __future__ import annotations

from typing import Iterable

from .api import PluginAPI


_INSTANCE: PluginAPI | None = None


def get_api() -> PluginAPI:
    global _INSTANCE
    if _INSTANCE is None:
        _INSTANCE = PluginAPI()
        discover_and_register(_INSTANCE)
    return _INSTANCE


def discover_and_register(api: PluginAPI) -> None:
    for register in _entry_points():
        try:
            register(api)
        except Exception as exc:
            print(f"tmiki: plugin failed to register: {exc}")


def _entry_points() -> Iterable[callable]:
    try:
        from importlib.metadata import entry_points
    except ImportError:  # pragma: no cover - very old Python
        return []
    eps = entry_points()
    # The shape of the returned value differs across Python versions.
    if hasattr(eps, "select"):
        chosen = eps.select(group="tmi.plugins")
    else:
        chosen = eps.get("tmi.plugins", [])
    out = []
    for ep in chosen:
        try:
            out.append(ep.load())
        except Exception as exc:
            print(f"tmiki: could not load plugin {ep.name}: {exc}")
    return out
