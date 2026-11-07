"""Public types for tmi plugins.

A plugin's entry point function takes a single ``PluginAPI`` argument and
calls ``api.on(event, handler)`` to subscribe to lifecycle events. Handlers
receive a ``PluginContext`` describing the current operation.

This module is intentionally tiny in v1; the surface area expands as more
event sources become reliable. Plugins should treat anything not listed below
as unstable.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Mapping


SUPPORTED_EVENTS = (
    "post-commit",
    "post-merge",
    "post-switch",
    # Reserved for future use by the auto-commit extension:
    "post-save",
)


@dataclass
class PluginContext:
    event: str
    repo_dir: Path
    worktree: Path | None
    payload: Mapping[str, Any] = field(default_factory=dict)


Handler = Callable[[PluginContext], None]


class PluginAPI:
    """Subscription + invocation surface handed to ``register(api)``."""

    def __init__(self) -> None:
        self._handlers: dict[str, list[Handler]] = {ev: [] for ev in SUPPORTED_EVENTS}

    def on(self, event: str, handler: Handler) -> None:
        if event not in SUPPORTED_EVENTS:
            raise RuntimeError(
                f"unknown plugin event: {event!r}. "
                f"Supported events: {', '.join(SUPPORTED_EVENTS)}"
            )
        self._handlers[event].append(handler)

    def emit(self, event: str, context: PluginContext) -> None:
        for handler in self._handlers.get(event, []):
            try:
                handler(context)
            except Exception as exc:  # plugin errors must not corrupt the core
                print(f"tmiki: plugin handler for {event} raised: {exc}")
