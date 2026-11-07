"""Plugin entry points for tmi.

Third-party packages register ``tmi.plugins`` entry points whose targets are
callables ``register(api: PluginAPI) -> None``. tmi discovers them lazily and
invokes ``register`` once per process so plugins can subscribe to events such
as ``post-commit``.

The first planned out-of-core extension is auto-commit (time/operation-based
snapshotting), kept out of MVP per project conventions.
"""

from __future__ import annotations

from .api import PluginAPI, PluginContext
from .loader import discover_and_register

__all__ = ["PluginAPI", "PluginContext", "discover_and_register"]
