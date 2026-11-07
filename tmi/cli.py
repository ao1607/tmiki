from __future__ import annotations

import argparse
import fnmatch
import functools
import itertools
import os
import re
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from . import attributes as _attributes
from . import config as _config
from . import diskspace as _diskspace
from . import hooks as _hooks
from . import lockfile as _lockfile
from . import pixel_limit as _pixel_limit
from . import reflog as _reflog
from . import textsafe as _textsafe
from .layer_props import format_change, format_opacity
from .repo import DEFAULT_BRANCH, Repo, iter_supported_files, read_file_digest, same_entry, split_pathspec


BRAILLE_FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
ASCII_FRAMES = "|/-\\"


def _pick_frames(stream) -> str:
    enc = getattr(stream, "encoding", None) or ""
    try:
        BRAILLE_FRAMES.encode(enc)
        return BRAILLE_FRAMES
    except (UnicodeEncodeError, LookupError):
        return ASCII_FRAMES


_TRUE = {"1", "true", "yes", "on"}
_FALSE = {"0", "false", "no", "off"}
_PHASE_WORD = re.compile(r"^[a-z]+$")


def _isatty(stream) -> bool:
    try:
        return bool(stream.isatty())
    except (AttributeError, ValueError):
        return False


def _phase_key(msg: str) -> str:
    """The phase a progress message belongs to: its leading lowercase words
    (at most two), so "decoding layers 3/40" and "decoding layers 4/40", or
    "writing layer a.clip:x" and "writing layer a.clip:y", are one phase."""
    words: list[str] = []
    for word in msg.split():
        if len(words) == 2 or not _PHASE_WORD.match(word):
            break
        words.append(word)
    return " ".join(words) or msg


class Progress:
    """Phase/elapsed reporting on stderr for long operations.

    On a TTY (stderr and stdout both terminals): a single spinner line,
    "<frame> tmiki: <phase> (<elapsed>s)", redrawn in place and erased when
    done. With ``TMI_PROGRESS=1`` otherwise: one line per phase change (at
    most one per second within a phase) and a closing per-phase time
    breakdown. Otherwise — and always with ``TMI_PROGRESS=0`` — silent.

    The Rust front door (rust/crates/tmi-cli progress.rs) prints the same.
    When it hands a pull to Python after its fetch, ``TMI_PROGRESS_RESUME``
    carries its clock and phases on, so the lines and the breakdown read
    as one run.
    """

    def __init__(self, stream=None, interval: float = 0.08):
        self.stream = stream if stream is not None else sys.stderr
        isatty = _isatty(self.stream) and _isatty(sys.stdout)
        env = os.environ.get("TMI_PROGRESS", "").strip().lower()
        if env in _FALSE:
            self.mode = None
        elif isatty:
            self.mode = "tty"
        elif env in _TRUE:
            self.mode = "lines"
        else:
            self.mode = None
        self.tty = self.mode == "tty"
        self.interval = interval
        self.message = ""
        self.last_len = 0
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.frames = itertools.cycle(_pick_frames(self.stream))
        self.thread: threading.Thread | None = None
        self.started = time.monotonic()
        self.phase: str | None = None
        self.phase_started = self.started
        self.phase_times: dict[str, float] = {}
        self.last_line_phase: str | None = None
        self.last_line_at = 0.0
        resume = os.environ.pop("TMI_PROGRESS_RESUME", None)
        if resume:
            self._resume(resume)
        if self.tty:
            self.thread = threading.Thread(target=self._spin, daemon=True)
            self.thread.start()

    def _resume(self, text: str) -> None:
        import json

        try:
            state = json.loads(text)
            now = time.monotonic()
            self.started = now - float(state["elapsed"])
            if state.get("phase") is not None:
                self.phase = str(state["phase"])
                self.phase_started = now - float(state["phase_elapsed"])
            self.phase_times = {str(name): float(sec) for name, sec in state.get("times", [])}
            self.last_line_phase = state.get("last_line_phase")
            if state.get("last_line_ago") is not None:
                self.last_line_at = now - float(state["last_line_ago"])
        except (ValueError, KeyError, TypeError):
            pass

    def _spin(self) -> None:
        while not self.stop_event.wait(self.interval):
            with self.lock:
                if self.message and not self.stop_event.is_set():
                    self._draw_locked(next(self.frames))

    def _draw_locked(self, frame: str) -> None:
        elapsed = time.monotonic() - self.started
        text = f"{frame} tmiki: {self.message} ({elapsed:.1f}s)"
        pad = " " * max(0, self.last_len - len(text))
        self.stream.write(f"\r{text}{pad}")
        self.stream.flush()
        self.last_len = len(text)

    def _enter_phase_locked(self, key: str, now: float) -> None:
        if key == self.phase:
            return
        if self.phase is not None:
            self.phase_times[self.phase] = self.phase_times.get(self.phase, 0.0) + now - self.phase_started
        self.phase = key
        self.phase_started = now

    def __call__(self, msg: str) -> None:
        if self.mode is None:
            return
        with self.lock:
            now = time.monotonic()
            key = _phase_key(msg)
            self._enter_phase_locked(key, now)
            if self.tty:
                self.message = msg
                self._draw_locked(next(self.frames))
                return
            if key != self.last_line_phase or now - self.last_line_at >= 1.0:
                self.stream.write(f"tmiki: [{now - self.started:6.1f}s] {msg}\n")
                self.stream.flush()
                self.last_line_phase = key
                self.last_line_at = now

    def clear(self) -> None:
        if not self.tty:
            return
        with self.lock:
            if self.last_len:
                pad = " " * self.last_len
                self.stream.write(f"\r{pad}\r")
                self.stream.flush()
                self.last_len = 0
            self.message = ""

    def done(self) -> None:
        if self.mode is None:
            return
        if self.mode == "lines":
            with self.lock:
                if self.phase is None:
                    return
                now = time.monotonic()
                self._enter_phase_locked("", now)
                self.phase = None
                parts = ", ".join(f"{name} {sec:.1f}s" for name, sec in self.phase_times.items() if name)
                self.stream.write(f"tmiki: done in {now - self.started:.1f}s ({parts})\n")
                self.stream.flush()
            return
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=0.5)
            self.thread = None
        self.clear()


def _with_progress(message: str, fn):
    """``fn()`` as one phase: commands that rewrite working files (switch,
    checkout, reset --hard, stash apply, merge --abort, gc) say what they
    are doing while they do it."""
    progress = Progress()
    try:
        progress(message)
        return fn()
    finally:
        progress.done()


def _releases_locks(command):
    """A command that takes index.lock (issue #35) gives it back when it returns or raises,
    also when the command is called as a function and not through main()."""

    @functools.wraps(command)
    def run(args: argparse.Namespace) -> int:
        try:
            return command(args)
        finally:
            _lockfile.release_all()

    return run


_PULL_FETCHED: str | None = None


def main(argv: list[str] | None = None) -> int:
    # The Rust front door (rust/crates/tmi-cli) sets this when it hands a
    # command to Python; dropping it here keeps hooks that run `tmi` from
    # inheriting it (the executable stops if it ever sees it: a loop).
    os.environ.pop("TMI_FORWARDED", None)
    # A pull the front door hands over after its fetch (RUST_MIGRATION.md
    # 4b): the remote it fetched, so cmd_pull does not fetch again.
    global _PULL_FETCHED
    _PULL_FETCHED = os.environ.pop("TMI_PULL_FETCHED", None)
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(errors="backslashreplace")
        except Exception:
            pass
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        try:
            return args.func(args)
        finally:
            # index.lock, if the command took it (issue #35); also on Ctrl+C
            _lockfile.release_all()
    except Exception as exc:
        if type(exc).__name__ == "DecompressionBombError":
            # Only if Pillow's own guard fired despite tmi.pixel_limit.
            sizes = re.findall(r"\((\d+) pixels\)", str(exc))
            if sizes:
                exc = _pixel_limit.CanvasTooLargeError(int(sizes[0]), _pixel_limit.current())
        if _diskspace.is_disk_full(exc):
            # Not [Errno 28] (issue #42); `add` says more, see Repo.stage_file.
            where = f" while writing {exc.filename}" if getattr(exc, "filename", None) else ""
            exc = RuntimeError(f"the disk is full{where}; free some space and run the command again")
        print(f"tmiki: error: {exc}", file=sys.stderr)
        return 1
    except BaseException as exc:
        # tmi.accel.AccelUnavailableError: tmi_accel is missing or too old;
        # tmi.image_ops.PillowUnavailableError: an image needs Pillow, which
        # is missing. Not Exceptions, so no fallback swallows them; matched
        # by name so that commands which never load those modules do not
        # import them here.
        if type(exc).__name__ not in ("AccelUnavailableError", "PillowUnavailableError"):
            raise
        print(f"tmiki: error: {exc}", file=sys.stderr)
        return 1


def _version_text() -> str:
    """tmi's version and tmi_accel's, for bug reports. tmi_accel is looked
    up by its installed metadata, not imported (that would load numpy)."""
    import importlib.metadata
    import importlib.util

    from . import __version__

    accel = "not installed (tmi needs it; see Install in the README)"
    if importlib.util.find_spec("tmi_accel") is not None:
        accel = "installed (version unknown)"
        for name in ("tmi-accel", "tmi_accel"):
            try:
                accel = importlib.metadata.version(name)
                break
            except importlib.metadata.PackageNotFoundError:
                continue
    return f"tmiki {__version__}\ntmi_accel {accel}"


class _VersionAction(argparse.Action):
    def __init__(self, option_strings, dest=argparse.SUPPRESS, default=argparse.SUPPRESS, help=None) -> None:
        super().__init__(option_strings=option_strings, dest=dest, default=default, nargs=0, help=help)

    def __call__(self, parser, namespace, values, option_string=None) -> None:
        print(_version_text())
        parser.exit()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="tmiki", description="tmiki")
    parser.add_argument("--version", action=_VersionAction, help="show tmi's and tmi_accel's versions and exit")
    sub = parser.add_subparsers(dest="command", required=True)

    init = sub.add_parser("init", help="create a .tmi repository")
    init.add_argument("path", nargs="?", default=".")
    init.add_argument("--bare", action="store_true")
    init.set_defaults(func=cmd_init)

    status = sub.add_parser("status", help="show working tree status")
    status.add_argument("-s", "--short", action="store_true", help="show short status")
    status.add_argument("-b", "--branch", action="store_true", help="show branch in short status")
    status.add_argument("--porcelain", nargs="?", const="v1", choices=["v1"], help="machine-readable short status")
    status.set_defaults(func=cmd_status)

    add = sub.add_parser("add", help="stage illustration files")
    add.add_argument("-A", "--all", action="store_true", help="stage all changes, including untracked files")
    add.add_argument("-u", "--update", action="store_true", help="stage modified/deleted tracked files only")
    add.add_argument("-f", "--force", action="store_true", help="allow adding otherwise ignored files")
    add.add_argument("--renormalize", action="store_true",
                     help="import tracked files again even when unchanged (after .tmiattributes or a plugin changed)")
    add.add_argument("specs", nargs="*")
    add.set_defaults(func=cmd_add)

    commit = sub.add_parser("commit", help="create a commit")
    commit.add_argument("-m", "--message")
    commit.add_argument("-a", "--all", dest="stage_all", action="store_true",
                        help="auto-stage modified tracked files before committing")
    commit.add_argument("--amend", action="store_true",
                        help="replace the previous commit with a new one (parents preserved)")
    commit.add_argument("--allow-empty", action="store_true",
                        help="create a commit even if no paths changed")
    commit.set_defaults(func=cmd_commit)

    log = sub.add_parser("log", help="show commit history")
    log.add_argument("--oneline", action="store_true", help="compact one-line output")
    log.add_argument("--graph", action="store_true", help="draw an ASCII commit graph")
    log.add_argument("--decorate", nargs="?", const="short", choices=["short", "full", "no"],
                     help="show branch/tag names")
    log.add_argument("--no-decorate", dest="decorate", action="store_const", const="no")
    log.add_argument("--all", action="store_true", help="walk all refs")
    log.add_argument("--first-parent", action="store_true", help="follow only the first parent")
    log.add_argument("-n", "--max-count", type=int, help="limit number of commits")
    log.add_argument("--skip", type=int, default=0, help="skip commits before showing output")
    log.add_argument("--reverse", action="store_true", help="show oldest commit first")
    log.add_argument("--author", help="limit commits by author substring")
    log.add_argument("--grep", help="limit commits by message regex")
    log.add_argument("--since", "--after", dest="since", help="show commits after this date")
    log.add_argument("--until", "--before", dest="until", help="show commits before this date")
    log.add_argument("--merges", action="store_true", help="show only merge commits")
    log.add_argument("--no-merges", action="store_true", help="omit merge commits")
    log.add_argument("--parents", action="store_true", help="show parent commit ids")
    log.add_argument("--abbrev", type=int, default=12, help="short hash length")
    log.add_argument("--abbrev-commit", dest="abbrev_commit", action="store_true",
                     help="abbreviate commit ids in full output")
    log.add_argument("--no-abbrev-commit", dest="abbrev_commit", action="store_false")
    log.set_defaults(abbrev_commit=False)
    log.add_argument("--format", dest="format_", help="pretty-print format, or format:<template>")
    log.add_argument("--pretty", help="pretty-print style or format:<template>")
    log.add_argument("--date", choices=["iso", "local", "short"], default="iso")
    log.add_argument("--stat", action="store_true", help="show changed path summary")
    log.add_argument("--name-only", action="store_true", help="show changed path names")
    log.add_argument("--name-status", action="store_true", help="show changed path names and status")
    log.add_argument("revs_or_paths", nargs="*")
    log.set_defaults(func=cmd_log)

    show = sub.add_parser("show", help="show a commit or file from a commit")
    show.add_argument("--stat", action="store_true", help="show changed path summary")
    show.add_argument("--name-only", action="store_true", help="show changed path names")
    show.add_argument("--name-status", action="store_true", help="show changed path names and status")
    show.add_argument("--abbrev", type=int, default=12, help="short hash length")
    show.add_argument("--date", choices=["iso", "local", "short"], default="iso")
    show.add_argument("--thumbnail", action="store_true", help="write the commit's composite thumbnail as a PNG")
    show.add_argument("-o", "--output", type=Path, help="with --thumbnail: where to write it (default: .tmi/thumbnails/<commit>-thumbnail.png)")
    show.add_argument("object", nargs="?", default="HEAD")
    show.set_defaults(func=cmd_show)

    diff = sub.add_parser(
        "diff",
        help="show changes with visual previews",
        description="tmiki diff [<rev> [<rev>]] [--staged] [[--] <path>...]: without revisions, "
        "the working tree against the index; --staged, the index against HEAD; one revision, "
        "that commit against the working tree; two, the two commits. So after 'tmiki add', "
        "see the change with --staged; after 'tmiki commit', with HEAD~1.",
    )
    group = diff.add_mutually_exclusive_group()
    group.add_argument("--side-by-side", action="store_true", help="write old | new PNGs (default)")
    group.add_argument("--overlay", action="store_true", help="write the new image over a faded old one")
    group.add_argument("--per-layer", action="store_true", help="write a highlighted difference image per changed layer")
    group.add_argument("--html", action="store_true", help="write an HTML report (previews embedded, originals linked)")
    diff.add_argument("--staged", "--cached", dest="staged", action="store_true", help="compare the index with HEAD (or the given revision)")
    diff.add_argument("--no-open", action="store_true", help="do not open the HTML report in a browser")
    diff.add_argument("--name-only", action="store_true", help="show only changed path names")
    diff.add_argument("--name-status", action="store_true", help="show changed path names and status letters")
    diff.add_argument("--stat", action="store_true", help="show a compact change summary (no image decoding)")
    diff.add_argument("--out", type=Path, help="directory for the preview images / report")
    diff.add_argument("args", nargs="*", metavar="rev-or-path")
    diff.set_defaults(func=cmd_diff)

    ls_layers = sub.add_parser(
        "ls-layers",
        help="list a file's layers with their folder paths and ids",
        description="Lists every layer of an illustration file: id, kind, visibility, opacity, blend mode "
        "and folder path. Any of the path, the id or a unique name selects a layer in file:layer specs.",
    )
    ls_layers.add_argument("path")
    ls_layers.add_argument("rev", nargs="?", help="commit to read (default: the working file)")
    ls_layers.set_defaults(func=cmd_ls_layers)

    branch = sub.add_parser("branch", help="list, create, or delete branches")
    branch.add_argument("-a", "--all", action="store_true",
                        help="list local and remote-tracking branches")
    branch.add_argument("-r", "--remotes", action="store_true",
                        help="list remote-tracking branches")
    branch.add_argument("-v", "--verbose", action="count", default=0,
                        help="show commit subject for each branch")
    branch.add_argument("--merged", nargs="?", const="HEAD",
                        help="list branches merged into the given revision")
    branch.add_argument("--no-merged", nargs="?", const="HEAD",
                        help="list branches not merged into the given revision")
    branch.add_argument("--contains", nargs="?", const="HEAD",
                        help="list branches containing the given revision")
    branch.add_argument("--sort", default="refname",
                        help="sort by refname, -refname, committerdate, or -committerdate")
    branch.add_argument("-d", "--delete", action="store_true",
                        help="delete a branch (only if merged into HEAD)")
    branch.add_argument("-D", dest="force_delete", action="store_true",
                        help="force-delete a branch even if not merged")
    branch.add_argument("name", nargs="?")
    branch.add_argument("start", nargs="?")
    branch.set_defaults(func=cmd_branch)

    switch = sub.add_parser("switch", help="switch branches")
    switch.add_argument("-c", "--create", action="store_true",
                        help="create the branch (at HEAD or <start>) before switching")
    switch.add_argument("name")
    switch.add_argument("start", nargs="?",
                        help="with -c, the revision to start the new branch at")
    switch.add_argument("--discard", action="store_true", help="overwrite tracked working files")
    switch.set_defaults(func=cmd_switch)

    restore = sub.add_parser("restore", help="restore files or layer PNGs from a commit")
    restore.add_argument("-S", "--staged", action="store_true",
                         help="unstage the given paths (reset the index to HEAD); during a merge, "
                              "a conflicted file staged with 'tmiki add' becomes unresolved again")
    restore.add_argument("--source", help="commit, branch or tag to restore from (default: the index); MERGE_HEAD works "
                                          "during a merge; WORKTREE, with --output, exports from the working file "
                                          "including changes not yet added")
    restore.add_argument("--in-place", action="store_true", help="write a restored layer back into the native file")
    restore.add_argument("--output", type=Path, help="write to this path instead (read from the index unless --source "
                                                     "is given; --source WORKTREE for the working file): file:layer as "
                                                     "a PNG; a whole file as it is, or its image as a PNG when the path "
                                                     "ends in .png")
    restore_side = restore.add_mutually_exclusive_group()
    restore_side.add_argument("--ours", action="store_true",
                              help="during a merge: take our side of a conflicted file, or of one layer with file:layer")
    restore_side.add_argument("--theirs", action="store_true",
                              help="during a merge: take their side of a conflicted file, or of one layer with file:layer")
    restore.add_argument("specs", nargs="+")
    restore.set_defaults(func=cmd_restore)

    checkout = sub.add_parser("checkout", help="switch branches or restore paths")
    checkout.add_argument("-b", "--branch", help="create and switch to a new branch")
    checkout.add_argument("--discard", action="store_true", help="overwrite tracked working files")
    checkout.add_argument("args", nargs="*")
    checkout.set_defaults(func=cmd_checkout)

    reset = sub.add_parser("reset", help="reset HEAD or the index")
    reset_mode = reset.add_mutually_exclusive_group()
    reset_mode.add_argument("--soft", action="store_true")
    reset_mode.add_argument("--mixed", action="store_true")
    reset_mode.add_argument("--hard", action="store_true")
    reset.add_argument("args", nargs="*")
    reset.set_defaults(func=cmd_reset)

    rm = sub.add_parser("rm", help="remove paths from the working tree and index")
    rm.add_argument("-f", "--force", action="store_true")
    rm.add_argument("--cached", action="store_true", help="remove only from the index")
    rm.add_argument("specs", nargs="+")
    rm.set_defaults(func=cmd_rm)

    mv = sub.add_parser("mv", help="move or rename a path")
    mv.add_argument("-f", "--force", action="store_true")
    mv.add_argument("source")
    mv.add_argument("dest")
    mv.set_defaults(func=cmd_mv)

    tag = sub.add_parser("tag", help="list, create, or delete tags")
    tag.add_argument("-a", "--annotated", action="store_true")
    tag.add_argument("-m", "--message")
    tag.add_argument("-d", "--delete", action="store_true", help="delete a tag")
    tag.add_argument("-l", "--list", action="store_true", help="list tags matching an optional pattern")
    tag.add_argument("-n", nargs="?", const=1, type=str, dest="lines",
                     help="show annotation lines when listing")
    tag.add_argument("--points-at", help="list tags pointing at the given revision")
    tag.add_argument("--contains", help="list tags whose target contains the given revision")
    tag.add_argument("--sort", default="refname",
                     help="sort by refname, -refname, creatordate, or -creatordate")
    tag.add_argument("name", nargs="?")
    tag.add_argument("rev", nargs="?")
    tag.set_defaults(func=cmd_tag)

    remote = sub.add_parser("remote", help="manage remotes")
    remote.add_argument("-v", "--verbose", action="store_true", help="show fetch/push URLs")
    remote_sub = remote.add_subparsers(dest="remote_command")
    remote_add = remote_sub.add_parser("add")
    remote_add.add_argument("name")
    remote_add.add_argument("url")
    remote_add.set_defaults(func=cmd_remote_add)
    remote_remove = remote_sub.add_parser("remove", aliases=["rm"])
    remote_remove.add_argument("name")
    remote_remove.set_defaults(func=cmd_remote_remove)
    remote_rename = remote_sub.add_parser("rename")
    remote_rename.add_argument("old")
    remote_rename.add_argument("new")
    remote_rename.set_defaults(func=cmd_remote_rename)
    remote_set_url = remote_sub.add_parser("set-url")
    remote_set_url.add_argument("name")
    remote_set_url.add_argument("url")
    remote_set_url.set_defaults(func=cmd_remote_set_url)
    remote.set_defaults(func=cmd_remote_list)

    fetch = sub.add_parser("fetch", help="fetch objects and refs from a remote")
    fetch.add_argument("--all", action="store_true", help="fetch all remotes")
    fetch.add_argument("remote", nargs="?", default="origin")
    fetch.set_defaults(func=cmd_fetch)

    push = sub.add_parser("push", help="push a branch or tags to a remote")
    push.add_argument("remote", nargs="?", default="origin")
    push.add_argument("branch", nargs="?", help="the branch (default: the current one), or a tag, to push")
    push.add_argument("--tags", action="store_true", help="push every tag instead of a branch")
    push_force = push.add_mutually_exclusive_group()
    push_force.add_argument("-f", "--force", action="store_true",
                            help="overwrite a non-fast-forward remote branch")
    push_force.add_argument("--force-with-lease", action="store_true",
                            help="overwrite only if the remote still matches the local tracking ref")
    push.set_defaults(func=cmd_push)

    pull = sub.add_parser("pull", help="fetch and merge from a remote")
    pull.add_argument("remote", nargs="?", default="origin")
    pull.add_argument("branch", nargs="?")
    pull.set_defaults(func=cmd_pull)

    clone = sub.add_parser("clone", help="clone a tmi repository (local path or tmihub HTTPS URL)")
    clone.add_argument("url")
    clone.add_argument("directory", nargs="?")
    clone.set_defaults(func=cmd_clone)

    merge = sub.add_parser("merge", help="merge another branch or commit")
    merge.add_argument("rev", nargs="?")
    merge.add_argument("-m", "--message")
    merge.add_argument("--abort", action="store_true",
                       help="give up a conflicted merge and return to HEAD (edits made while resolving are backed up)")
    merge.set_defaults(func=cmd_merge)

    stash = sub.add_parser("stash", help="stash local changes")
    stash_sub = stash.add_subparsers(dest="stash_command")
    stash_push = stash_sub.add_parser("push", help="save local changes")
    stash_push.add_argument("-m", "--message")
    stash_push.add_argument("message_words", nargs="*")
    stash_push.set_defaults(func=cmd_stash)
    stash_save = stash_sub.add_parser("save", help="save local changes")
    stash_save.add_argument("message_words", nargs="*")
    stash_save.set_defaults(func=cmd_stash)
    stash_list = stash_sub.add_parser("list", help="list stash entries")
    stash_list.set_defaults(func=cmd_stash)
    stash_apply = stash_sub.add_parser("apply", help="apply a stash entry")
    stash_apply.add_argument("spec", nargs="?")
    stash_apply.set_defaults(func=cmd_stash)
    stash_pop = stash_sub.add_parser("pop", help="apply and drop a stash entry")
    stash_pop.add_argument("spec", nargs="?")
    stash_pop.set_defaults(func=cmd_stash)
    stash_drop = stash_sub.add_parser("drop", help="drop a stash entry")
    stash_drop.add_argument("spec", nargs="?")
    stash_drop.set_defaults(func=cmd_stash)
    stash_clear = stash_sub.add_parser("clear", help="drop all stash entries")
    stash_clear.set_defaults(func=cmd_stash)
    stash.set_defaults(func=cmd_stash)

    hook = sub.add_parser("hook", help="inspect or run installed hooks")
    hook_sub = hook.add_subparsers(dest="hook_command")
    hook_list = hook_sub.add_parser("list", help="list installed hooks")
    hook_list.set_defaults(func=cmd_hook_list)
    hook_run = hook_sub.add_parser("run", help="manually invoke an installed hook")
    hook_run.add_argument("name", choices=_hooks.SUPPORTED_HOOKS)
    hook_run.set_defaults(func=cmd_hook_run)
    hook.set_defaults(func=cmd_hook_list)

    gc = sub.add_parser("gc", help="delete unreachable objects (reflog-protected)")
    gc.add_argument("--prune", default="90d",
                    help="reflog cutoff: 'now', or '<N>d' for N days (default: 90d)")
    gc.set_defaults(func=cmd_gc)

    recover = sub.add_parser(
        "recover",
        aliases=["reflog"],
        help="list HEAD movements and restore lost commits onto a new branch",
    )
    recover.add_argument(
        "--as",
        dest="as_branch",
        help="create a branch at the given revision (HEAD@{N}, oid, or branch name)",
    )
    recover.add_argument("rev", nargs="?", help="revision to recover (default: list reflog)")
    recover.add_argument("--files", action="store_true",
                         help="list working files tmi backed up before overwriting them (restore, switch --discard, reset --hard, merge --abort)")
    recover.add_argument("--file", dest="backup", metavar="N",
                         help="put backup N (from --files) back at its path, or at --output")
    recover.add_argument("--output", type=Path, help="with --file: write the backup here instead")
    recover.set_defaults(func=cmd_recover)

    config = sub.add_parser("config", help="get or set tmiki config values")
    config.add_argument("--global", dest="global_scope", action="store_true",
                        help="operate on the per-user config instead of the repo config")
    config.add_argument("--list", "-l", dest="list_all", action="store_true",
                        help="list all effective keys")
    config.add_argument("--unset", action="store_true", help="remove the given key")
    config.add_argument("key", nargs="?",
                        help="section.name, e.g. user.name, or core.maxImagePixels (largest image tmi "
                             "decodes, in pixels; 0 = no limit; default from installed memory)")
    config.add_argument("value", nargs="?")
    config.set_defaults(func=cmd_config)

    # PLUGINS.md P6: run by the tmi executable only (format plugins are
    # wasm components it hosts); the parser is here for -h and argparse's
    # errors, cmd_plugin refuses.
    plugin = sub.add_parser("plugin", help="this repository's format plugins (.tmiplugins/, .tmiattributes)")
    plugin_sub = plugin.add_subparsers(dest="plugin_command")
    plugin_add = plugin_sub.add_parser("add", help="put a component in .tmiplugins/ and map files to it in .tmiattributes")
    plugin_add.add_argument("file", help="the component (.wasm), or an installed plugin: <id>[@<version>]")
    plugin_add.add_argument("patterns", nargs="*",
                            help="files it reads, e.g. '*.ora' (default: the extensions the plugin names)")
    plugin_add.add_argument("--force", action="store_true", help="replace a newer version of the plugin")
    plugin_sub.add_parser("list", help="list the built-in formats and this repository's plugins")
    plugin_sub.add_parser("prepare", help="check, compile and trust this repository's plugins ahead of use")
    plugin_reimport = plugin_sub.add_parser("reimport", help="import again every tracked file a plugin reads")
    plugin_reimport.add_argument("id")
    plugin_trust = plugin_sub.add_parser("trust", help="let a plugin read and write files of this repository without asking")
    plugin_trust.add_argument("id")
    plugin_trust.add_argument("--revoke", action="store_true", help="take that back")
    plugin_remove = plugin_sub.add_parser(
        "remove",
        help="stop using a plugin: remove its component and its .tmiattributes mapping, and record the files it read without it",
    )
    plugin_remove.add_argument("id")
    # P8: the executable hands `plugin dev <program> ...` to itself before
    # argparse (the program's own options are not tmi's); this is for -h
    plugin_dev = plugin_sub.add_parser(
        "dev",
        help="try a development plugin (an executable speaking the plugin interface on stdin/stdout; "
             "its records cannot be committed)",
    )
    plugin_dev.add_argument("program", help="the development plugin, e.g. target/debug/ora-dev")
    plugin_dev.add_argument("args", nargs="*",
                            help="its arguments; after --, a tmi command to run with it, e.g. -- add a.ora "
                                 "(without one: start it and say what it is)")
    # PLUGINS.md P8: the plugin author's tools (outside any repository)
    plugin_new = plugin_sub.add_parser("new", help="write a plugin's directory: a template that builds and passes 'tmiki plugin test'")
    plugin_new.add_argument("id", help="the plugin's id ([a-z0-9-], at most 40)")
    plugin_new.add_argument("--lang", metavar="LANG",
                            help="the template's language: rust, the only one (tmi plugins are written in Rust)")
    plugin_new.add_argument("--writer", action="store_true", help="also write layers back (world format-plugin)")
    plugin_build = plugin_sub.add_parser("build", help="build the plugin directory's component with cargo, then check it")
    plugin_build.add_argument("dir", nargs="?", help="the plugin directory (default: .)")
    plugin_test = plugin_sub.add_parser("test", help="check a plugin against its corpus: every failure is listed")
    plugin_test.add_argument("path", nargs="?", help="the plugin directory (default: .) or a component (.wasm)")
    plugin_test.add_argument("--corpus", help="the sample files (default: tmi-plugin.toml's)")
    plugin_test.add_argument("--native", action="store_true",
                             help="also compare with the plugin as a development plugin (tmiki plugin dev): "
                                  "tmi-plugin.toml's [dev] command")
    plugin_test.add_argument("--native-command", metavar="CMD",
                             help="like --native, with this command (words, or a JSON array of strings)")
    plugin_test.add_argument("--against", metavar="OLD", help="list how the records differ from an older component's")
    plugin_test.add_argument("--update", action="store_true", help="write the *.expect.json files")
    plugin_test.add_argument("--fuzz", type=int, metavar="N", help="also import N damaged copies of the corpus")
    plugin_test.add_argument("--dump", action="store_true", help="print each file's structure")
    plugin_doctor = plugin_sub.add_parser("doctor", help="say what building a plugin needs and what is installed")
    plugin_doctor.add_argument("dir", nargs="?", help="a plugin directory (what its build needs decides the exit code)")
    plugin_install = plugin_sub.add_parser("install", help="keep a component in ~/.tmi/plugins/ for 'tmiki plugin add <id>'")
    plugin_install.add_argument("file", help="the component (.wasm)")
    plugin.set_defaults(func=cmd_plugin)

    return parser


def cmd_plugin(args: argparse.Namespace) -> int:
    print(
        "tmiki: error: 'tmiki plugin' runs in the tmi executable only; python -m tmi does not run format plugins",
        file=sys.stderr,
    )
    return 1


def cmd_init(args: argparse.Namespace) -> int:
    repo = Repo.create(Path(args.path), bare=args.bare)
    print(f"Initialized empty tmi repository in {repo.repo_dir}")
    return 0


def cmd_status(args: argparse.Namespace) -> int:
    repo = Repo.open()
    branch = repo.current_branch() or "(detached)"
    head = repo.current_commit()
    if args.short or args.porcelain:
        return _print_short_status(repo, branch, head, show_branch=args.branch)

    print(f"On branch {branch}")
    print(f"HEAD {head[:12] if head else '(no commits)'}")
    merge_head = repo.pending_merge_head()
    conflict_paths: set[str] = set()
    if merge_head:
        print(f"You are in the middle of a merge (MERGE_HEAD {merge_head[:12]}).")
        conflicts = repo.pending_conflict_paths()
        conflict_paths = set(conflicts)
        if conflicts:
            print("Unresolved conflicts (resolve, then 'tmiki add <file>' and 'tmiki commit'; or 'tmiki merge --abort'):")
            for path in conflicts:
                print(f"  conflict {_textsafe.escape(path)}")
                # What `restore --ours/--theirs` already took (issue #15).
                choices = repo.conflict_choices(path)
                for item in choices:
                    layer = item.get("path_name") or item.get("name")
                    line = f"    {layer}: {item.get('reason')}" if layer else f"    {item.get('reason')}"
                    if item.get("taken"):
                        line += f" -> took {item['taken']}"
                    print(line)
                taken = [item for item in choices if item.get("taken")]
                edited = bool(choices) and choices[0].get("edited_since")
                if taken and edited:
                    print("    (working file edited since then)")
                elif edited:
                    print("    (working file edited since the merge)")
                if choices and len(taken) == len(choices):
                    print(f"    every conflict has a side; check the file, then 'tmiki add {path}'")
        else:
            print("All conflicts resolved; stage your changes and run 'tmiki commit'.")
    progress = Progress()
    try:
        items = repo.status_items(on_progress=progress)
    finally:
        progress.done()
    items = [item for item in items if item.path not in conflict_paths]
    if not items:
        if merge_head:
            # The conflicted files are listed above, not here.
            print("no other changes" if conflict_paths else "no unstaged changes")
            return 0
        print("nothing to commit, working tree clean")
        return 0
    for item in items:
        detail = f"  {item.detail}" if item.detail else ""
        print(f"{item.state:10} {item.path}{detail}")
    return 0


def _print_short_status(repo: Repo, branch: str, head: str | None, *, show_branch: bool) -> int:
    if show_branch:
        print(f"## {branch}")
    conflict_paths = set(repo.pending_conflict_paths())
    for path in sorted(conflict_paths):
        print(f"UU {_textsafe.escape(path)}")
    # Short/porcelain output uses only item.state/item.path (see
    # _staged_status_letter), never a modified file's layer detail, so skip the
    # expensive per-file layer decode.
    items = [
        item
        for item in repo.status_items(with_layer_detail=False)
        if item.path not in conflict_paths
    ]
    head_entries = repo.entries_for_commit(head)
    # status_items can emit both a staged and a worktree item for the same path
    # (edited again after staging); git shows that as a single XY code.
    states_by_path: dict[str, dict[str, Any]] = {}
    for item in items:
        states_by_path.setdefault(item.path, {})[item.state] = item
    for path in sorted(states_by_path):
        states = states_by_path[path]
        if "untracked" in states:
            print(f"?? {_textsafe.escape(path)}")
            continue
        staged_item = states.get("staged")
        x = _staged_status_letter(staged_item, head_entries) if staged_item is not None else " "
        if "modified" in states:
            y = "M"
        elif "deleted" in states:
            y = "D"
        else:
            y = " "
        print(f"{x}{y} {_textsafe.escape(path)}")
    return 0


def _staged_status_letter(item, head_entries: dict[str, dict[str, Any]]) -> str:
    if item.detail == "delete":
        return "D"
    if item.path not in head_entries:
        return "A"
    return "M"


@_releases_locks
def cmd_add(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    renormalize = getattr(args, "renormalize", False)
    if not args.specs and not (args.all or args.update or renormalize):
        raise RuntimeError("add requires pathspecs, or -A/--all")
    # tmi does not track symbolic links or junctions; one named on the
    # command line is an error, as Git refuses a path beyond one (issue #44).
    for spec in args.specs:
        if any(ch in spec for ch in "*?["):
            continue
        link = repo.pathspec_link(split_pathspec(spec)[0])
        if link is not None:
            kind, link_path, is_spec = link
            if is_spec:
                raise RuntimeError(
                    f"{spec} is a {kind}; tmi does not track symbolic links or junctions "
                    "(add the file it points to instead)"
                )
            raise RuntimeError(
                f"{spec} is beyond the {kind} {link_path}; tmi does not follow symbolic links or junctions "
                "(use the real path)"
            )
    progress = Progress()
    try:
        if renormalize:
            # PLUGINS.md 4.3: tracked files imported again, unchanged or not
            if repo.pending_merge_head():
                raise RuntimeError("add --renormalize during a merge: finish or abort the merge first")
            root = repo.require_worktree()
            head_entries = repo.entries_for_commit(repo.current_commit())
            index_entries = repo.read_index().get("entries", {})
            known = set(head_entries) | set(index_entries)
            filters = [_make_path_filter(repo, spec, known) for spec in args.specs]
            staged: list[str] = []
            failures: list[tuple[str, str]] = []
            matched_any = False
            for rel in sorted(known):
                if filters and not any(match(rel) for match in filters):
                    continue
                entry = index_entries.get(rel) or head_entries.get(rel)
                if entry is not None and entry.get("deleted"):
                    continue
                matched_any = True
                full = root / Path(rel)
                if not full.is_file():
                    continue
                try:
                    _rel, _layer, changed = repo.stage_file(str(full), on_progress=progress, force=True)
                except Exception as exc:
                    # a file that cannot be imported is listed at the end; the
                    # rest go on (issues #37, #49)
                    if getattr(exc, "stops_add", False):
                        raise
                    failures.append((rel, _failure_reason(exc)))
                    continue
                if changed:
                    staged.append(rel)
            if args.specs and not matched_any:
                raise RuntimeError("pathspec did not match any files")
            return _report_bulk_add(progress, staged, failures)
        if args.all or args.update:
            failures: list[tuple[str, str]] = []
            staged, matched_any = _stage_matching_paths(
                repo,
                args.specs,
                include_untracked=args.all,
                on_progress=progress,
                failures=failures,
            )
            if args.specs and not matched_any:
                raise RuntimeError("pathspec did not match any files")
            return _report_bulk_add(progress, staged, failures, repo)

        if any(_is_bulk_add_spec(repo, spec) for spec in args.specs):
            failures = []
            staged, matched_any = _stage_matching_paths(
                repo, args.specs, include_untracked=True, on_progress=progress, failures=failures
            )
            if args.specs and not matched_any:
                raise RuntimeError("pathspec did not match any files")
            return _report_bulk_add(progress, staged, failures, repo)

        pending_conflicts = set(repo.pending_conflict_paths())
        for spec in args.specs:
            untouched = None
            if pending_conflicts:
                file_part, _layer = split_pathspec(spec)
                try:
                    conflict_rel = repo.pathspec_relpath(file_part, pending_conflicts)
                except Exception:
                    conflict_rel = None
                if conflict_rel in pending_conflicts:
                    untouched = repo.conflict_left_untouched(conflict_rel)
            rel, layer, changed = _stage_named(repo, spec, on_progress=progress, force=args.force)
            progress.clear()
            if untouched:
                if any(c.get("stable_source_id") for c in untouched):
                    names = ", ".join(str(c.get("path_name") or c.get("name") or c.get("reason")) for c in untouched)
                    print(
                        f"warning: {rel} was staged exactly as the merge left it, so the conflicting "
                        f"layers keep our version: {names} (their side is dropped)",
                        file=sys.stderr,
                    )
                else:
                    # A conflict of the whole file (no layers): issue #43.
                    print(
                        f"warning: {rel} was staged exactly as the merge left it, so it keeps "
                        "our version (their side is dropped)",
                        file=sys.stderr,
                    )
            if changed:
                print(f"staged {rel}:{layer}" if layer else f"staged {rel}")
                if not layer:
                    for warning in _attributes.removed_component_warnings(repo, [rel]):
                        print(warning, file=sys.stderr)
    finally:
        progress.done()
    return 0


def _stage_named(repo: Repo, spec: str, **kwargs):
    """`repo.stage_file`, with the file's name in a canvas-too-large error
    (issue #37)."""
    try:
        return repo.stage_file(spec, **kwargs)
    except _pixel_limit.CanvasTooLargeError as exc:
        if exc.subject is not None:
            raise
        file_part = split_pathspec(spec)[0]
        try:
            subject = repo.relpath(repo.pathspec_full_path(file_part))
        except RuntimeError:
            subject = file_part
        raise _pixel_limit.CanvasTooLargeError(exc.pixels, exc.limit, subject) from exc


def _failure_reason(exc: BaseException) -> str:
    """Why a file could not be added, in a line that names no path (the list
    names it): an OS error in tmi's words, else the error's own text."""
    if isinstance(exc, FileNotFoundError):
        return "the file is gone"
    if isinstance(exc, PermissionError):
        return "cannot read it (permission denied, or another program has it open)"
    if isinstance(exc, OSError) and exc.strerror:
        return exc.strerror
    return " ".join(str(exc).split())


def _report_bulk_add(progress, staged: list[str], failures: list[tuple[str, str]], repo: Repo | None = None) -> int:
    """Print the staged files, then the ones that could not be (stderr); 1 if
    there are any, as Git's `add` is when it refuses a path (issue #37)."""
    for rel in staged:
        progress.clear()
        print(f"staged {rel}")
    if repo is not None:
        for warning in _attributes.removed_component_warnings(repo, staged):
            print(warning, file=sys.stderr)
    if not failures:
        return 0
    progress.clear()
    count = len(failures)
    print(f"tmiki: error: {count} {'file' if count == 1 else 'files'} could not be added:", file=sys.stderr)
    for rel, reason in failures:
        print(f"  {rel}: {reason.removeprefix(rel + ': ')}", file=sys.stderr)
    return 1


def _is_bulk_add_spec(repo: Repo, spec: str) -> bool:
    file_part, layer = split_pathspec(spec)
    if layer is not None:
        return False
    if any(ch in file_part for ch in "*?["):
        return True
    full = repo.pathspec_full_path(file_part)
    return full.exists() and full.is_dir()


def _stage_matching_paths(
    repo: Repo,
    specs: list[str],
    *,
    include_untracked: bool,
    on_progress=None,
    failures: list[tuple[str, str]] | None = None,
) -> tuple[list[str], bool]:
    """Stage what ``specs`` match. With ``failures``, a file that cannot be
    staged is noted there as ``(path, reason)`` and the rest go on (issue #37);
    a full disk still stops at once."""
    root = repo.require_worktree()
    head_entries = repo.entries_for_commit(repo.current_commit())
    index_entries = repo.read_index().get("entries", {})
    known_paths = set(head_entries) | set(index_entries)
    filters = [_make_path_filter(repo, spec, known_paths) for spec in specs]

    def matches(rel: str) -> bool:
        return not filters or any(match(rel) for match in filters)

    staged: list[str] = []
    matched_any = False
    for rel in sorted(set(head_entries) | set(index_entries)):
        if not matches(rel):
            continue
        matched_any = True
        entry = index_entries.get(rel)
        head_entry = head_entries.get(rel)
        full = root / Path(rel)
        if not full.exists():
            if not (entry is not None and entry.get("deleted")):
                repo.stage_delete(rel)
                staged.append(rel)
            continue
        if entry is not None and not entry.get("deleted"):
            target = entry
        elif head_entry is not None:
            target = head_entry
        else:
            target = None
        source = None
        try:
            if target is not None:
                # Read and hashed once: the import takes these bytes.
                source = read_file_digest(full)
                if source[1] == target.get("source_digest"):
                    continue
            _rel, _layer, changed = _stage_named(repo, str(full), on_progress=on_progress, source=source)
        except Exception as exc:
            if failures is None or getattr(exc, "stops_add", False):
                raise
            failures.append((rel, _failure_reason(exc)))
            continue
        source = None
        if changed:
            staged.append(rel)

    if include_untracked:
        ignored = repo.parsed_ignore_patterns()
        tracked = set(head_entries) | set(index_entries)
        links: list[tuple[str, str]] = []
        for full in sorted(iter_supported_files(root, ignored, links)):
            rel = repo.relpath(full)
            if rel in tracked or not matches(rel):
                continue
            matched_any = True
            try:
                _rel, _layer, changed = _stage_named(repo, str(full), on_progress=on_progress)
            except Exception as exc:
                if failures is None or getattr(exc, "stops_add", False):
                    raise
                failures.append((rel, _failure_reason(exc)))
                continue
            if changed:
                staged.append(rel)
        # Not tracked, and said so (issue #44): a link is neither staged nor
        # silently an alias of its target.
        for link_rel, kind in sorted(links):
            if matches(link_rel):
                matched_any = True
                if on_progress is not None:
                    on_progress.clear()
                print(
                    f"warning: {link_rel} is a {kind}; tmi does not track symbolic links or "
                    "junctions, so it was skipped",
                    file=sys.stderr,
                )
    return staged, matched_any


def _make_path_filter(repo: Repo, spec: str, known_paths: set[str]):
    if any(ch in spec for ch in "*?["):
        pattern = spec.replace("\\", "/")
        return lambda rel: fnmatch.fnmatch(rel, pattern)

    full = repo.pathspec_full_path(spec)
    root = repo.require_worktree()
    if full.exists() and full.is_dir():
        rel_dir = "" if full.resolve() == root.resolve() else repo.relpath(full)
        prefix = rel_dir.rstrip("/")
        return lambda rel: not prefix or rel == prefix or rel.startswith(prefix + "/")

    rel = repo.pathspec_relpath(spec, known_paths)
    return lambda item: item == rel


@_releases_locks
def cmd_commit(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    if args.amend and repo.pending_merge_head():
        raise RuntimeError("cannot amend during a merge; finish the merge with 'tmiki commit' or discard it")
    progress = Progress()
    try:
        if args.stage_all:
            staged_paths = repo.stage_all_modified(on_progress=progress)
            progress.clear()
            for path in staged_paths:
                print(f"staged {path}")
            for warning in _attributes.removed_component_warnings(repo, staged_paths):
                print(warning, file=sys.stderr)
        if args.amend:
            oid = repo.commit_amend(message=args.message)
            label = "amend"
        else:
            if not args.message:
                raise RuntimeError("commit requires -m <message>")
            oid = repo.commit(args.message, allow_empty=args.allow_empty)
            label = ""
    finally:
        progress.done()
    branch = repo.current_branch() or "detached"
    if args.message:
        display_msg = first_line(args.message)
    else:
        display_msg = first_line(repo.read_commit(oid).get("message", ""))
    suffix = f" [{label}]" if label else ""
    print(f"[{branch} {oid[:12]}]{suffix} {display_msg}")
    return 0


def cmd_log(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.merges and args.no_merges:
        raise RuntimeError("--merges and --no-merges cannot be combined")
    starts, path_filters = _parse_log_revs_and_paths(repo, args)
    commits = _collect_log_commits(repo, starts, first_parent=args.first_parent)
    commits = _filter_log_commits(repo, commits, args, path_filters)
    if args.skip:
        commits = commits[max(0, args.skip):]
    if args.max_count is not None:
        commits = commits[: max(0, args.max_count)]
    if args.reverse:
        commits = list(reversed(commits))
    if not commits:
        print("no commits")
        return 0

    decorations = _commit_decorations(repo, full=args.decorate == "full")
    show_decorations = args.decorate not in (None, "no")
    graph = _LogGraph(first_parent=args.first_parent) if args.graph else None
    for oid, commit in commits:
        prefixes = _no_graph
        if graph:
            for line in graph.take_pending():
                _print_log_line(line)
            prefixes = graph.prefixes(oid, commit)
        _print_log_commit(
            repo,
            oid,
            commit,
            args,
            decorations.get(oid, []) if show_decorations else [],
            path_filters,
            prefixes,
        )
    if graph:
        for line in graph.take_pending():
            _print_log_line(line)
    return 0


def cmd_show(args: argparse.Namespace) -> int:
    repo = Repo.open()
    target = args.object or "HEAD"
    if args.thumbnail:
        oid = repo.resolve_rev(target)
        if not oid:
            raise RuntimeError(f"unknown revision: {target}")
        ref = repo.read_commit(oid).get("thumbnail_ref")
        if not ref:
            raise RuntimeError(f"commit {oid[:12]} has no thumbnail")
        from .image_ops import layer_blob_to_png

        # Not into the working tree by default: there it showed up as an
        # untracked file and `add -A` picked it up (issue #15).
        out = args.output or repo.repo_dir / "thumbnails" / f"{oid[:12]}-thumbnail.png"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(layer_blob_to_png(repo.store.read_blob(ref)))
        print(f"wrote {out if args.output else out.resolve()}")
        return 0
    if ":" in target and not target.startswith(":"):
        rev, path = target.split(":", 1)
        commit = repo.resolve_rev(rev or "HEAD")
        if not commit:
            raise RuntimeError(f"unknown revision: {rev}")
        entries = repo.entries_for_commit(commit)
        rel = repo.pathspec_relpath(path, set(entries))
        entry = entries.get(rel)
        if not entry:
            raise RuntimeError(f"path not found in {commit[:12]}: {rel}")
        sys.stdout.buffer.write(repo.store.read_blob(entry["source_blob_ref"]))
        return 0

    oid = repo.resolve_rev(target)
    if not oid:
        raise RuntimeError(f"unknown revision: {target}")
    commit = repo.read_commit(oid)
    log_args = argparse.Namespace(
        format_=None,
        pretty=None,
        oneline=False,
        parents=True,
        abbrev=args.abbrev,
        abbrev_commit=False,
        date=args.date,
        stat=args.stat,
        name_only=args.name_only,
        name_status=args.name_status or not args.stat and not args.name_only,
    )
    decorations = _commit_decorations(repo).get(oid, [])
    _print_log_commit(repo, oid, commit, log_args, decorations, [], _no_graph)
    return 0


def _no_graph() -> str:
    return ""


class _GraphPrefixes:
    """The graph column in front of each line one commit prints: the commit
    line's first, then the lines that move columns after it (taken from
    the graph's pending list, so what this commit does not use is printed
    before the next one), then plain padding."""

    def __init__(self, commit_line: str, pending: list[str], padding: str) -> None:
        self.commit_line: str | None = commit_line
        self.pending = pending
        self.padding = padding

    def __call__(self) -> str:
        if self.commit_line is not None:
            line, self.commit_line = self.commit_line, None
            return line
        if self.pending:
            return self.pending.pop(0)
        return self.padding


class _LogGraph:
    """Git's commit graph (graph.c), reduced to what tmi draws (issue #31).

    ``columns`` are the commits the lines wait for, left to right. A commit
    takes the first column waiting for it, or a new one on the right (a
    branch tip met for the first time). Its parents replace it there; a
    merge fans them out to the right (``|\\``), shifting the columns right
    of it. A column waiting for a commit another column already waits for
    closes in from the right (``|/``), one column per line. Every line of
    one commit is padded to the same width."""

    def __init__(self, *, first_parent: bool = False) -> None:
        self.columns: list[str] = []
        self.pending: list[str] = []
        self.first_parent = first_parent

    def take_pending(self) -> list[str]:
        """Lines of the previous commit's column moves not printed yet."""
        lines, self.pending = self.pending, []
        return lines

    def prefixes(self, oid: str, commit: dict[str, Any]) -> _GraphPrefixes:
        old = list(self.columns)
        if oid in old:
            col = old.index(oid)
        else:
            old.append(oid)
            col = len(old) - 1
        parents = [parent for parent in (commit.get("parents") or []) if parent]
        if self.first_parent:
            parents = parents[:1]
        extra = max(len(parents) - 1, 0)
        new: list[str] = []

        def target(item: str) -> int:
            if item not in new:
                new.append(item)
            return new.index(item)

        # (position after the merge's fan-out line, column it ends in)
        edges: list[tuple[int, int]] = []
        for idx, item in enumerate(old):
            if idx == col:
                edges.extend((col + j, target(parent)) for j, parent in enumerate(parents))
            else:
                edges.append((idx + extra if idx > col else idx, target(item)))
        width = 2 * max(len(old), len(new), max((pos for pos, _ in edges), default=-1) + 1)
        commit_line = (" ".join("*" if idx == col else "|" for idx in range(len(old))) + " ").ljust(width)
        lines: list[str] = []
        if extra:
            chars = [" "] * width
            for idx in range(len(old)):
                if idx < col:
                    chars[2 * idx] = "|"
                elif idx == col:
                    chars[2 * idx] = "|"
                    for j in range(1, len(parents)):
                        chars[2 * (col + j) - 1] = "\\"
                else:
                    chars[2 * (idx + extra) - 1] = "\\"
            lines.append("".join(chars))
        while any(pos > end for pos, end in edges):
            chars = [" "] * width
            moved: list[tuple[int, int]] = []
            for pos, end in edges:
                if pos > end:
                    pos -= 1
                    chars[2 * pos + 1] = "/"
                else:
                    chars[2 * pos] = "|"
                moved.append((pos, end))
            edges = moved
            lines.append("".join(chars))
        self.columns = new
        self.pending = lines
        padding = (" ".join("|" for _ in new) + " ").ljust(width) if new else " " * width
        return _GraphPrefixes(commit_line, self.pending, padding)


def _parse_log_revs_and_paths(repo: Repo, args: argparse.Namespace) -> tuple[list[str], list[str]]:
    values = list(args.revs_or_paths or [])
    if args.all:
        return _all_log_start_oids(repo), values
    if not values:
        current = repo.current_commit()
        return ([current] if current else []), []
    try:
        return [repo.resolve_rev(values[0]) or ""], values[1:]
    except RuntimeError:
        first = values[0]
        plausible_path = (
            any(ch in first for ch in "*?[")
            or (Path.cwd() / first).exists()
            or (repo.require_worktree() / first).exists()
            or any(
                _path_matches(tracked, [first])
                for tracked in repo.entries_for_commit(repo.current_commit())
            )
        )
        if not plausible_path:
            raise
        current = repo.current_commit()
        return ([current] if current else []), values


def _all_log_start_oids(repo: Repo) -> list[str]:
    starts: list[str] = []
    current = repo.current_commit()
    if current:
        starts.append(current)
    for value in repo.all_refs().values():
        commit = _object_as_commit(repo, value)
        if commit:
            starts.append(commit)
    return list(dict.fromkeys(starts))


def _object_as_commit(repo: Repo, oid: str | None) -> str | None:
    if not oid:
        return None
    try:
        return repo._resolve_object_as_commit(oid, oid)
    except RuntimeError:
        return None


def _collect_log_commits(repo: Repo, starts: list[str], *, first_parent: bool) -> list[tuple[str, dict[str, Any]]]:
    collected: dict[str, dict[str, Any]] = {}
    stack = [oid for oid in starts if oid]
    while stack:
        cur = stack.pop()
        if cur in collected:
            continue
        commit = repo.read_commit(cur)
        collected[cur] = commit
        parents = list(commit.get("parents") or [])
        stack.extend(parents[:1] if first_parent else parents)
    return sorted(collected.items(), key=lambda kv: kv[1].get("timestamp", ""), reverse=True)


def _filter_log_commits(
    repo: Repo,
    commits: list[tuple[str, dict[str, Any]]],
    args: argparse.Namespace,
    path_filters: list[str],
) -> list[tuple[str, dict[str, Any]]]:
    since = _parse_log_date(args.since) if args.since else None
    until = _parse_log_date(args.until) if args.until else None
    author_filter = args.author.casefold() if args.author else None
    grep = re.compile(args.grep) if args.grep else None
    out: list[tuple[str, dict[str, Any]]] = []
    for oid, commit in commits:
        parents = commit.get("parents") or []
        if args.merges and len(parents) < 2:
            continue
        if args.no_merges and len(parents) >= 2:
            continue
        timestamp = _commit_datetime(commit)
        if since and timestamp and timestamp < since:
            continue
        if until and timestamp and timestamp > until:
            continue
        if author_filter:
            author = commit.get("author", {})
            text = f"{author.get('name', '')} {author.get('email', '')}".casefold()
            if author_filter not in text:
                continue
        if grep and not grep.search(commit.get("message", "")):
            continue
        if path_filters and not _commit_touches_paths(repo, oid, commit, path_filters):
            continue
        out.append((oid, commit))
    return out


def _parse_log_date(value: str) -> datetime:
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        if re.fullmatch(r"\d{4}-\d{2}-\d{2}", text):
            dt = datetime.fromisoformat(text).replace(tzinfo=timezone.utc)
        else:
            dt = datetime.fromisoformat(text)
    except ValueError as exc:
        raise RuntimeError(f"could not parse date: {value}") from exc
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _commit_datetime(commit: dict[str, Any]) -> datetime | None:
    raw = commit.get("timestamp")
    if not isinstance(raw, str) or not raw:
        return None
    try:
        text = raw[:-1] + "+00:00" if raw.endswith("Z") else raw
        dt = datetime.fromisoformat(text)
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def _commit_touches_paths(repo: Repo, oid: str, commit: dict[str, Any], filters: list[str]) -> bool:
    return any(_path_matches(path, filters) for _status, path in _commit_changes(repo, oid, commit))


def _path_matches(path: str, filters: list[str]) -> bool:
    for raw in filters:
        pattern = raw.replace("\\", "/")
        if any(ch in pattern for ch in "*?[") and fnmatch.fnmatch(path, pattern):
            return True
        prefix = pattern.rstrip("/")
        if path == prefix or path.startswith(prefix + "/"):
            return True
    return False


def _commit_changes(repo: Repo, oid: str, commit: dict[str, Any]) -> list[tuple[str, str]]:
    parents = commit.get("parents") or []
    before = repo.entries_for_commit(parents[0]) if parents else {}
    after = repo.entries_for_commit(oid)
    changes: list[tuple[str, str]] = []
    for path in sorted(set(before) | set(after)):
        old = before.get(path)
        new = after.get(path)
        if old is None and new is not None:
            changes.append(("A", path))
        elif old is not None and new is None:
            changes.append(("D", path))
        elif not same_entry(old, new):
            changes.append(("M", path))
    return changes


def _commit_decorations(repo: Repo, *, full: bool = False) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    current_branch = repo.current_branch()
    current_commit = repo.current_commit()
    for ref, value in sorted(repo.all_refs().items()):
        commit = _object_as_commit(repo, value)
        if not commit:
            continue
        label = ref if full else _short_ref_name(ref)
        if ref.startswith("refs/heads/") and ref.removeprefix("refs/heads/") == current_branch:
            target = ref if full else current_branch
            label = f"HEAD -> {target}"
        out.setdefault(commit, []).append(label)
    if current_commit and not current_branch:
        out.setdefault(current_commit, []).insert(0, "HEAD")
    return out


def _short_ref_name(ref: str) -> str:
    if ref.startswith("refs/heads/"):
        return ref.removeprefix("refs/heads/")
    if ref.startswith("refs/remotes/"):
        return ref.removeprefix("refs/remotes/")
    if ref.startswith("refs/tags/"):
        return "tag: " + ref.removeprefix("refs/tags/")
    return ref


def _print_log_commit(
    repo: Repo,
    oid: str,
    commit: dict[str, Any],
    args: argparse.Namespace,
    decorations: list[str],
    path_filters: list[str],
    prefixes: Any,
) -> None:
    """``prefixes()`` gives the graph column for each line, in order."""
    pretty = args.format_ or args.pretty
    if pretty and pretty.startswith("format:"):
        pretty = pretty.removeprefix("format:")
    elif pretty and pretty not in {"oneline", "short", "medium", "full"}:
        # git log --format="%h %s" treats the value itself as a format string.
        pass
    elif args.oneline:
        pretty = "oneline"

    if pretty and pretty not in {"short", "medium", "full", "oneline"}:
        rendered = _render_log_format(repo, oid, commit, pretty, decorations, args)
        for line in rendered.splitlines() or [""]:
            _print_log_line(prefixes(), line)
        _print_commit_change_summary(repo, oid, commit, args, path_filters, prefixes)
        return

    if pretty == "oneline" or args.oneline:
        ids = [_short_oid(oid, args.abbrev)]
        if args.parents:
            ids.extend(_short_oid(parent, args.abbrev) for parent in commit.get("parents") or [])
        deco = f" ({', '.join(decorations)})" if decorations else ""
        _print_log_line(prefixes(), f"{' '.join(ids)}{deco} {first_line(commit.get('message', ''))}")
        _print_commit_change_summary(repo, oid, commit, args, path_filters, prefixes)
        return

    commit_id = _short_oid(oid, args.abbrev) if args.abbrev_commit else oid
    parents = commit.get("parents") or []
    parent_text = ""
    if args.parents:
        shown = [_short_oid(parent, args.abbrev) if args.abbrev_commit else parent for parent in parents]
        parent_text = " " + " ".join(shown) if shown else ""
    deco = f" ({', '.join(decorations)})" if decorations else ""
    _print_log_line(prefixes(), f"commit {commit_id}{parent_text}{deco}")
    if len(parents) > 1:
        _print_log_line(prefixes(), "Merge: " + " ".join(_short_oid(parent, args.abbrev) for parent in parents))
    author = commit.get("author", {})
    _print_log_line(prefixes(), f"Author: {author.get('name', '')} <{author.get('email', '')}>")
    _print_log_line(prefixes(), f"Date:   {_format_commit_date(commit, args.date)}")
    if commit.get("thumbnail_ref"):
        _print_log_line(prefixes(), f"Thumbnail: tmiki show --thumbnail {_short_oid(oid, args.abbrev)}")
    _print_log_line(prefixes())
    for line in commit.get("message", "").splitlines():
        _print_log_line(prefixes(), f"    {line}")
    _print_log_line(prefixes())
    _print_commit_change_summary(repo, oid, commit, args, path_filters, prefixes)


def _render_log_format(
    repo: Repo,
    oid: str,
    commit: dict[str, Any],
    template: str,
    decorations: list[str],
    args: argparse.Namespace,
) -> str:
    author = commit.get("author", {})
    parents = list(commit.get("parents") or [])
    sentinel = "\0TMI_PERCENT\0"
    rendered = template.replace("%%", sentinel)
    replacements = {
        "%H": oid,
        "%h": _short_oid(oid, args.abbrev),
        "%P": " ".join(parents),
        "%p": " ".join(_short_oid(parent, args.abbrev) for parent in parents),
        "%s": first_line(commit.get("message", "")),
        "%B": commit.get("message", ""),
        "%an": str(author.get("name", "")),
        "%ae": str(author.get("email", "")),
        "%ad": _format_commit_date(commit, args.date),
        "%D": ", ".join(decorations),
        "%d": f" ({', '.join(decorations)})" if decorations else "",
        "%n": "\n",
    }
    for token in sorted(replacements, key=len, reverse=True):
        rendered = rendered.replace(token, replacements[token])
    return rendered.replace(sentinel, "%")


def _print_commit_change_summary(
    repo: Repo,
    oid: str,
    commit: dict[str, Any],
    args: argparse.Namespace,
    path_filters: list[str],
    prefixes: Any,
) -> None:
    if not (args.stat or args.name_only or args.name_status):
        return
    changes = [
        (status, path)
        for status, path in _commit_changes(repo, oid, commit)
        if not path_filters or _path_matches(path, path_filters)
    ]
    if not changes:
        return
    if args.name_only:
        for _status, path in changes:
            _print_log_line(prefixes(), path)
        return
    if args.name_status:
        for status, path in changes:
            _print_log_line(prefixes(), f"{status}\t{path}")
        return
    for status, path in changes:
        label = {"A": "new file", "M": "modified", "D": "deleted"}[status]
        _print_log_line(prefixes(), f" {path} | {label}")
    noun = "file" if len(changes) == 1 else "files"
    _print_log_line(prefixes(), f" {len(changes)} {noun} changed")


def _print_log_line(prefix: str, line: str = "") -> None:
    line = _textsafe.escape(line)
    if line:
        print(f"{prefix}{line}")
    elif prefix:
        print(prefix.rstrip())
    else:
        print("")


def _format_commit_date(commit: dict[str, Any], mode: str) -> str:
    raw = str(commit.get("timestamp", ""))
    dt = _commit_datetime(commit)
    if not dt:
        return raw
    if mode == "short":
        return dt.date().isoformat()
    if mode == "local":
        return dt.astimezone().isoformat(sep=" ", timespec="seconds")
    return raw


def _short_oid(oid: str, length: int = 12) -> str:
    return oid[: max(4, length)]


def cmd_ls_layers(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.rev:
        commit = repo.resolve_rev(args.rev)
        entries = repo.entries_for_commit(commit)
        rel = repo.pathspec_relpath(args.path, set(entries))
        if rel not in entries:
            raise RuntimeError(f"path not found in {args.rev}: {rel}")
        layers = repo.flatten_layers(entries[rel]["tree_ref"])
    else:
        full = repo.pathspec_full_path(args.path)
        if not full.is_file():
            raise RuntimeError(f"no such file: {args.path}")
        rel = repo.relpath(full)
        known = dict(repo.entries_for_commit(repo.current_commit()))
        known.update({p: e for p, e in repo.read_index().get("entries", {}).items() if e.get("tree_ref")})
        entry = known.get(rel)
        source, digest = read_file_digest(full)
        if entry and digest == entry.get("source_digest"):
            layers = repo.flatten_layers(entry["tree_ref"])
        else:
            from .importers import import_file, walk_imported_layers_with_paths

            imported = import_file(full, cache=repo.render_cache, source=source, source_digest=digest)
            layers = [
                {
                    "path_name": path,
                    "layer_id": layer.layer_id,
                    "kind": layer.kind,
                    "visible": layer.visible,
                    "opacity": layer.opacity,
                    "blend_mode": layer.blend_mode,
                }
                for path, layer in walk_imported_layers_with_paths(imported.layers)
            ]
    print(f"{'id':14} {'kind':10} {'shown':5} {'opacity':>7}  {'blend':12} path")
    for layer in layers:
        opacity_text = format_opacity(layer.get("opacity"))
        print(
            f"{str(layer.get('layer_id')):14} {str(layer.get('kind') or ''):10} "
            f"{'yes' if layer.get('visible') else 'no':5} {opacity_text:>7}  "
            f"{str(layer.get('blend_mode') or ''):12} {layer.get('path_name')}"
        )
    return 0


def _diff_sides_from_args(repo: Repo, args: argparse.Namespace):
    """Git's forms: ``diff`` (index vs working tree), ``diff --staged``
    (HEAD vs index), ``diff <rev>`` (rev vs working tree), ``diff <a> <b>``,
    each optionally followed by ``[--] <path>...``."""
    from .diff import WORKTREE, DiffSide, index_side

    values = list(args.args or [])
    if "--" in values:
        sep = values.index("--")
        revs, paths = values[:sep], values[sep + 1:]
    else:
        revs, paths = [], []
        for i, value in enumerate(values):
            try:
                resolved = repo.resolve_rev(value) if len(revs) < 2 else None
            except RuntimeError:
                resolved = None
            if resolved and not _pathspec_exists_or_tracked(repo, value):
                revs.append(value)
            else:
                paths = values[i:]
                break
    if len(revs) > 2:
        raise RuntimeError("diff takes at most two revisions")

    def commit_side(rev: str) -> DiffSide:
        oid = repo.resolve_rev(rev)
        if not oid:
            raise RuntimeError(f"unknown revision: {rev}")
        return DiffSide(oid, rev)

    if args.staged:
        if len(revs) > 1:
            raise RuntimeError("--staged takes at most one revision")
        head = revs[0] if revs else "HEAD"
        return commit_side(head), index_side(), paths
    if len(revs) == 2:
        return commit_side(revs[0]), commit_side(revs[1]), paths
    if len(revs) == 1:
        return commit_side(revs[0]), WORKTREE, paths
    if not repo.current_commit() and not repo.read_index().get("entries"):
        raise RuntimeError("no commit to diff against")
    return index_side(), WORKTREE, paths


def cmd_diff(args: argparse.Namespace) -> int:
    repo = Repo.open()
    summary_only = args.name_only or args.name_status or args.stat
    if summary_only:
        mode = "summary" if args.stat else "names"
    elif args.html:
        mode = "html"
    elif args.overlay:
        mode = "overlay"
    elif args.per_layer:
        mode = "per-layer"
    else:
        mode = "side-by-side"
    from datetime import datetime as _dt

    auto_out = args.out is None
    out_dir = args.out or (repo.repo_dir / "diff" / _dt.now().strftime("%Y%m%d-%H%M%S"))
    from .diff import diff_sides

    left, right, paths = _diff_sides_from_args(repo, args)
    index_html: Path | None = None
    progress = Progress()
    try:
        results = diff_sides(repo, left, right, paths, mode=mode, out_dir=out_dir, on_progress=progress)
        if results and not summary_only:
            if auto_out:
                _prune_diff_dirs(repo.repo_dir / "diff", keep=_diff_retain_count(repo))
            if mode == "html":
                from .diff_html import render_report

                progress("writing HTML report")
                index_html = render_report(results, out_dir / "index.html")
    finally:
        progress.done()
    if not results:
        if not (args.name_only or args.name_status):
            print("no differences")
        if left.rev == ":index" and right.rev is None and _has_staged_changes(repo, paths):
            # Plain `tmiki diff` is working tree vs index, like Git: what was
            # added does not show here (issue #15).
            print("tmiki: hint: staged changes are not shown; use 'tmiki diff --staged' to see them", file=sys.stderr)
        if auto_out and out_dir.exists() and not any(out_dir.iterdir()):
            out_dir.rmdir()
        return 0
    if summary_only:
        _print_diff_summary(results, args)
        return 0
    if mode == "html":
        # Strip in-memory PNG bytes from results before printing so we don't leak base64 to stdout.
        for r in results:
            r.pop("html", None)
            for layer in r.get("layers", []):
                layer.pop("html", None)
    for result in results:
        print(f"diff {result['path']}")
        if result.get("binary"):
            print("  binary file changed")
        if result.get("bbox"):
            print(f"  composite bbox={result['bbox']}")
        if result.get("state") == "deleted":
            print("  deleted")
        for layer in result.get("layers", []):
            suffix = f" bbox={layer['bbox']}" if layer.get("bbox") else ""
            what = "pixels" if result.get("raster") else f"layer {layer['name']}"
            print(f"  {layer['state']} {what}{suffix}")
            for change in layer.get("prop_changes", []):
                print(f"    {format_change(change['key'], change.get('old'), change.get('new'))}")
        for output in result.get("outputs", []):
            print(f"  wrote {output}")
    if index_html is not None:
        print(f"wrote {index_html}")
        if not args.no_open:
            _open_in_default_browser(index_html)
    return 0


def _has_staged_changes(repo: Repo, paths: list[str]) -> bool:
    staged = set(repo.read_index().get("entries", {}))
    if not staged or not paths:
        return bool(staged)
    try:
        return any(repo.pathspec_relpath(spec, staged) in staged for spec in paths)
    except Exception:
        return False


def _print_diff_summary(results: list[dict[str, Any]], args: argparse.Namespace) -> None:
    if args.name_only:
        for result in results:
            print(result["path"])
        return
    if args.name_status:
        for result in results:
            print(f"{_diff_status_letter(result)}\t{result['path']}")
        return
    # Per file: what changed inside it, in layers (issue #24: "2 changes"
    # counted the file itself as a change on top of its one changed layer).
    for result in results:
        if result.get("binary"):
            detail = "binary"
        elif result.get("state") == "deleted":
            detail = "deleted"
        elif result.get("raster"):
            detail = "new image" if result.get("state") == "added" else "pixels changed"
        else:
            detail = _layer_count_text(result.get("layers") or [])
            if result.get("state") == "added":
                detail = f"new file, {detail}" if detail else "new file"
            elif not detail:
                detail = "modified, no layer changes"
        print(f" {result['path']} | {detail}")
    noun = "file" if len(results) == 1 else "files"
    print(f" {len(results)} {noun} changed")


def _layer_count_text(layers: list[dict[str, Any]]) -> str:
    """``2 layers changed, 1 layer added`` for a file's layer results."""
    parts = []
    for state in ("changed", "added", "deleted"):
        count = sum(1 for layer in layers if layer.get("state") == state)
        if count:
            parts.append(f"{count} {'layer' if count == 1 else 'layers'} {state}")
    return ", ".join(parts)


def _diff_status_letter(result: dict[str, Any]) -> str:
    state = result.get("state")
    if state == "deleted":
        return "D"
    if state == "added":
        return "A"
    return "M"


_DIFF_DIR_PATTERN = "[0-9]" * 8 + "-" + "[0-9]" * 6


def _diff_retain_count(repo: Repo) -> int:
    try:
        raw = _config.get_value(repo.read_config(), "diff.retain")
    except Exception:
        raw = None
    if raw is None:
        return 20
    try:
        value = int(raw)
    except (TypeError, ValueError):
        return 20
    return max(1, value)


def _prune_diff_dirs(root: Path, keep: int) -> None:
    if not root.exists():
        return
    dirs = sorted(
        (p for p in root.glob(_DIFF_DIR_PATTERN) if p.is_dir()),
        key=lambda p: p.name,
        reverse=True,
    )
    import shutil as _sh
    for stale in dirs[keep:]:
        _sh.rmtree(stale, ignore_errors=True)


def _open_in_default_browser(path: Path) -> None:
    # On Windows, webbrowser.open often resolves to IE because of stale registry
    # entries. Prefer the OS shell association so the user's actual default
    # browser is used.
    try:
        if sys.platform == "win32":
            import os as _os

            _os.startfile(str(path))  # type: ignore[attr-defined]
            return
        if sys.platform == "darwin":
            import subprocess as _sp

            _sp.Popen(["open", str(path)])
            return
        import subprocess as _sp

        _sp.Popen(["xdg-open", str(path)])
    except Exception as exc:  # pragma: no cover - depends on user env
        try:
            import webbrowser
            from urllib.request import pathname2url

            webbrowser.open("file:" + pathname2url(str(path)), new=2)
        except Exception:
            print(f"tmiki: could not open browser: {exc}", file=sys.stderr)


def cmd_branch(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.delete or args.force_delete:
        if not args.name:
            raise RuntimeError("branch -d requires a branch name")
        if args.start:
            raise RuntimeError("branch -d does not take a start revision")
        target = repo.delete_branch(args.name, force=args.force_delete)
        print(f"deleted branch {args.name} (was {target[:12]})")
        return 0
    if args.name:
        commit = repo.create_branch(args.name, args.start)
        print(f"created branch {args.name} at {commit[:12]}")
        return 0
    branches = _branch_list_rows(repo, args)
    if not branches:
        print(f"* {DEFAULT_BRANCH} (no commits)")
        return 0
    for name, commit, current, _ref in branches:
        marker = "*" if current else " "
        name = _textsafe.escape(name)
        if args.verbose:
            subject = _textsafe.escape(first_line(repo.read_commit(commit).get("message", ""))) if commit else ""
            print(f"{marker} {name:24} {(commit or '')[:12]} {subject}")
        else:
            print(f"{marker} {name:20} {(commit or '')[:12]}")
    return 0


def _branch_list_rows(repo: Repo, args: argparse.Namespace) -> list[tuple[str, str | None, bool, str]]:
    current = repo.current_branch()
    rows: list[tuple[str, str | None, bool, str]] = []
    if not args.remotes:
        for name, commit in repo.list_head_refs().items():
            rows.append((name, commit, name == current, f"refs/heads/{name}"))
    if args.all or args.remotes:
        for ref, value in sorted(repo.all_refs().items()):
            if not ref.startswith("refs/remotes/"):
                continue
            commit = _object_as_commit(repo, value)
            short = ref.removeprefix("refs/remotes/")
            name = short if args.remotes and not args.all else f"remotes/{short}"
            rows.append((name, commit, False, ref))

    contains = repo.resolve_rev(args.contains) if args.contains else None
    merged_into = repo.resolve_rev(args.merged) if args.merged else None
    no_merged_into = repo.resolve_rev(args.no_merged) if args.no_merged else None
    filtered: list[tuple[str, str | None, bool, str]] = []
    for row in rows:
        name, commit, is_current, ref = row
        if not commit:
            continue
        if contains and not repo.is_ancestor(contains, commit):
            continue
        if merged_into and not repo.is_ancestor(commit, merged_into):
            continue
        if no_merged_into and repo.is_ancestor(commit, no_merged_into):
            continue
        filtered.append((name, commit, is_current, ref))
    return _sort_branch_rows(repo, filtered, args.sort)


def _sort_branch_rows(
    repo: Repo,
    rows: list[tuple[str, str | None, bool, str]],
    sort_key: str,
) -> list[tuple[str, str | None, bool, str]]:
    reverse = sort_key.startswith("-")
    key = sort_key[1:] if reverse else sort_key
    if key in {"committerdate", "authordate"}:
        return sorted(rows, key=lambda row: _commit_sort_date(repo, row[1]), reverse=reverse)
    return sorted(rows, key=lambda row: row[0], reverse=reverse)


def _commit_sort_date(repo: Repo, oid: str | None) -> str:
    if not oid:
        return ""
    try:
        return str(repo.read_commit(oid).get("timestamp", ""))
    except Exception:
        return ""


@_releases_locks
def cmd_switch(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    backups_before = len(repo.read_backups())
    if args.create:
        if repo.read_ref(f"refs/heads/{args.name}") is not None:
            raise RuntimeError(f"branch already exists: {args.name}")
        repo.create_branch(args.name, args.start)
        try:
            commit = _with_progress(f"switching to {args.name}", lambda: repo.switch(args.name, discard=args.discard))
        except Exception:
            repo.delete_branch(args.name, force=True)
            raise
    else:
        if args.start is not None:
            raise RuntimeError("a start revision is only allowed with -c/--create")
        commit = _with_progress(f"switching to {args.name}", lambda: repo.switch(args.name, discard=args.discard))
    _report_backups(repo, backups_before)
    print(f"Switched to branch {args.name}")
    if commit:
        print(f"HEAD is now {commit[:12]}")
    _remind_reopen(repo.rewritten)
    return 0


@_releases_locks
def cmd_restore(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    if args.staged:
        if args.source or args.in_place or args.output:
            raise RuntimeError("--staged cannot be combined with --source, --in-place, or --output")
        merging = repo.pending_merge_head() is not None
        for spec in args.specs:
            rel, was_staged = repo.unstage(spec)
            if merging and was_staged and rel in repo.pending_conflict_paths():
                print(
                    f"unstaged {rel}; its merge conflict is unresolved again "
                    f"(tmiki restore --ours|--theirs {rel}[:<layer>], then 'tmiki add {rel}')"
                )
            else:
                print(f"unstaged {rel}" if was_staged else f"{rel} was not staged")
        return 0
    if args.ours or args.theirs:
        if args.source or args.in_place or args.output:
            raise RuntimeError("--ours/--theirs cannot be combined with --source, --in-place, or --output")
        side = "ours" if args.ours else "theirs"
        backups_before = len(repo.read_backups())
        progress = Progress()
        try:
            for spec in args.specs:
                progress(f"restoring {spec}")
                repo.resolve_conflict(spec, side)
                progress.clear()
                print(f"took {side} for {spec}; stage it with 'tmiki add' when done")
        finally:
            progress.done()
        _report_backups(repo, backups_before)
        _remind_reopen(repo.rewritten)
        return 0
    if args.output and len(args.specs) != 1:
        raise RuntimeError("--output can only be used with one restore spec")
    if args.output and args.in_place:
        raise RuntimeError("--output cannot be used with --in-place")
    backups_before = len(repo.read_backups())
    _restore_specs(args.specs, lambda spec: repo.restore(spec, source=args.source, in_place=args.in_place, output=args.output))
    _report_backups(repo, backups_before)
    _remind_reopen(repo.rewritten)
    return 0


def _restore_specs(specs: list[str], restore) -> None:
    """``restored <path>`` for each spec, one "restoring <spec>" phase each."""
    progress = Progress()
    try:
        for spec in specs:
            progress(f"restoring {spec}")
            out = restore(spec)
            progress.clear()
            print(f"restored {out}")
    finally:
        progress.done()


def _report_backups(repo: Repo, before: int) -> None:
    made = len(repo.read_backups()) - before
    if made > 0:
        print(f"backed up {made} file(s) with uncommitted work first; see 'tmiki recover --files'")


@_releases_locks
def cmd_checkout(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    backups_before = len(repo.read_backups())
    try:
        return _checkout(repo, args)
    finally:
        _report_backups(repo, backups_before)
        _remind_reopen(repo.rewritten)


def _checkout(repo: Repo, args: argparse.Namespace) -> int:
    values = list(args.args)
    if args.branch:
        start = values[0] if values else None
        if len(values) > 1:
            raise RuntimeError("checkout -b accepts at most one start revision")
        repo.create_branch(args.branch, start)
        try:
            commit = _with_progress(f"switching to {args.branch}", lambda: repo.switch(args.branch, discard=args.discard))
        except Exception:
            repo.delete_branch(args.branch, force=True)
            raise
        print(f"Switched to a new branch {args.branch}")
        if commit:
            print(f"HEAD is now {commit[:12]}")
        return 0

    if not values:
        raise RuntimeError("checkout requires a branch, revision, or pathspec")

    if "--" in values:
        sep = values.index("--")
        source_values = values[:sep]
        specs = values[sep + 1:]
        if not specs:
            raise RuntimeError("checkout -- requires pathspecs after --")
        source = source_values[0] if source_values else None
        operation = f"checkout {source} --" if source else "checkout --"
        _restore_specs(specs, lambda spec: repo.restore(spec, source=source, operation=operation))
        return 0

    if len(values) > 1:
        source, specs = values[0], values[1:]
        _restore_specs(specs, lambda spec: repo.restore(spec, source=source, operation=f"checkout {source}"))
        return 0

    target = values[0]
    if repo.read_ref(f"refs/heads/{target}") is not None:
        commit = _with_progress(f"switching to {target}", lambda: repo.switch(target, discard=args.discard))
        print(f"Switched to branch {target}")
        if commit:
            print(f"HEAD is now {commit[:12]}")
        return 0
    try:
        repo.resolve_rev(target)
        is_rev = True
    except RuntimeError:
        is_rev = False
    if not is_rev and _pathspec_exists_or_tracked(repo, target):
        out = _with_progress(f"restoring {target}", lambda: repo.restore(target, operation="checkout"))
        print(f"restored {out}")
        return 0
    commit = _with_progress(f"checking out {target}", lambda: repo.checkout_detached(target, discard=args.discard))
    print(f"HEAD is now at {commit[:12]}")
    return 0


def _pathspec_exists_or_tracked(repo: Repo, spec: str) -> bool:
    full = repo.pathspec_full_path(spec)
    if full.exists():
        return True
    try:
        rel = repo.relpath(full)
    except Exception:
        return False
    return rel in repo.entries_for_commit(repo.current_commit())


@_releases_locks
def cmd_reset(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    mode = "hard" if args.hard else "soft" if args.soft else "mixed"
    values = list(args.args or [])
    if len(values) > 1 or (values and "--" in values):
        if "--" in values:
            sep = values.index("--")
            source_values = values[:sep]
            specs = values[sep + 1:]
            source = source_values[0] if source_values else None
        else:
            source, specs = values[0], values[1:]
        if mode != "mixed":
            raise RuntimeError("path reset cannot be combined with --soft or --hard")
        if not specs:
            raise RuntimeError("reset requires pathspecs")
        for spec in specs:
            rel, changed = repo.reset_path(spec, source=source)
            print(f"unstaged {rel}" if changed else f"{rel} was not staged")
        return 0
    rev = values[0] if values else None
    backups_before = len(repo.read_backups())
    try:
        if mode == "hard":
            commit = _with_progress("resetting working tree", lambda: repo.reset(rev, mode=mode))
        else:
            commit = repo.reset(rev, mode=mode)
        _report_backups(repo, backups_before)
    except RuntimeError as exc:
        if (
            "unknown revision" in str(exc)
            and rev is not None
            and _reset_names_a_path(repo, rev)
        ):
            if mode != "mixed":
                raise RuntimeError("path reset cannot be combined with --soft or --hard")
            rel, changed = repo.reset_path(rev)
            print(f"unstaged {rel}" if changed else f"{rel} was not staged")
            return 0
        raise
    print(f"HEAD is now {commit[:12]}")
    _remind_reopen(repo.rewritten)
    return 0


def _reset_names_a_path(repo: Repo, spec: str) -> bool:
    if _pathspec_exists_or_tracked(repo, spec):
        return True
    try:
        rel = repo.relpath(repo.pathspec_full_path(spec))
    except Exception:
        return False
    return rel in repo.read_index().get("entries", {})


@_releases_locks
def cmd_rm(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    for spec in args.specs:
        rel = repo.remove_path(spec, cached=args.cached, force=args.force)
        print(f"removed {rel}" if not args.cached else f"removed {rel} from index")
        # a plugin's component that .tmiattributes still needs (issue #50)
        for warning in _attributes.removed_component_warnings(repo, [rel]):
            print(warning, file=sys.stderr)
    return 0


@_releases_locks
def cmd_mv(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    src, dst = repo.move_path(args.source, args.dest, force=args.force)
    print(f"renamed {src} -> {dst}")
    return 0


def cmd_tag(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.delete:
        if not args.name:
            raise RuntimeError("tag -d requires a tag name")
        if args.rev:
            raise RuntimeError("tag -d does not take a revision")
        target = repo.delete_tag(args.name)
        print(f"deleted tag {args.name} (was {target[:12]})")
        return 0
    if args.annotated and args.name and args.message is None:
        raise RuntimeError("annotated tag requires -m <message>")
    if args.lines is not None:
        if not str(args.lines).isdigit():
            if args.name is None:
                args.name = args.lines
            args.lines = 1
        else:
            args.lines = int(args.lines)
    listing = args.list or not args.name or args.points_at or args.contains or args.lines
    if listing:
        if args.rev:
            raise RuntimeError("tag listing accepts at most one pattern")
        for name, target in _tag_list_rows(repo, args):
            name = _textsafe.escape(name)
            if args.lines:
                annotation = _textsafe.escape(_tag_annotation(repo, target, args.lines))
                suffix = f" {annotation}" if annotation else ""
                print(f"{name:20} {target[:12]}{suffix}")
            else:
                print(f"{name:20} {target[:12]}")
        return 0
    message = args.message if args.annotated or args.message else None
    target = repo.create_tag(args.name, args.rev, message)
    print(f"tagged {args.name} -> {target[:12]}")
    return 0


def _tag_list_rows(repo: Repo, args: argparse.Namespace) -> list[tuple[str, str]]:
    pattern = args.name if args.name else None
    points_at = repo.resolve_rev(args.points_at) if args.points_at else None
    contains = repo.resolve_rev(args.contains) if args.contains else None
    rows: list[tuple[str, str, str, str]] = []
    for name, target in repo.list_tags():
        if pattern and not fnmatch.fnmatch(name, pattern):
            continue
        target_commit = _object_as_commit(repo, target)
        if points_at and target_commit != points_at:
            continue
        if contains and (not target_commit or not repo.is_ancestor(contains, target_commit)):
            continue
        rows.append((name, target, target_commit or "", _tag_sort_date(repo, target, target_commit)))
    reverse = args.sort.startswith("-")
    key = args.sort[1:] if reverse else args.sort
    if key == "creatordate":
        rows.sort(key=lambda row: row[3], reverse=reverse)
    else:
        rows.sort(key=lambda row: row[0], reverse=reverse)
    return [(name, target) for name, target, _commit, _date in rows]


def _tag_annotation(repo: Repo, target: str, line_count: int) -> str:
    try:
        object_type, _raw = repo.store.read(target)
    except Exception:
        return ""
    if object_type != "tag":
        return ""
    tag = repo.store.read_json(target, "tag")
    lines = tag.get("message", "").splitlines()[: max(0, line_count)]
    return " ".join(lines)


def _tag_sort_date(repo: Repo, target: str, target_commit: str | None) -> str:
    try:
        object_type, _raw = repo.store.read(target)
        if object_type == "tag":
            return str(repo.store.read_json(target, "tag").get("timestamp", ""))
    except Exception:
        pass
    return _commit_sort_date(repo, target_commit)


def cmd_remote_list(args: argparse.Namespace) -> int:
    repo = Repo.open()
    for name, value in sorted(repo.remotes().items()):
        url = value.get("url", "")
        if args.verbose:
            print(f"{name}\t{url} (fetch)")
            print(f"{name}\t{url} (push)")
        else:
            print(f"{name}\t{url}")
    return 0


def cmd_remote_add(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.add_remote(args.name, args.url)
    print(f"added remote {args.name} {args.url}")
    return 0


def cmd_remote_remove(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.remove_remote(args.name)
    print(f"removed remote {args.name}")
    return 0


def cmd_remote_rename(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.rename_remote(args.old, args.new)
    print(f"renamed remote {args.old} -> {args.new}")
    return 0


def cmd_remote_set_url(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.set_remote_url(args.name, args.url)
    print(f"set remote {args.name} url to {repo.remotes()[args.name]['url']}")
    return 0


def cmd_fetch(args: argparse.Namespace) -> int:
    repo = Repo.open()
    progress = Progress()
    try:
        if args.all:
            remotes = sorted(repo.remotes())
            if not remotes:
                print("no remotes")
                return 0
            for name in remotes:
                result = repo.fetch(name, on_progress=progress)
                progress.clear()
                print(f"fetched {result['objects']} objects from {name}")
                for branch, commit in sorted(result["refs"].items()):
                    print(f"  {name}/{branch} {commit[:12] if commit else '(empty)'}")
                for tag in result["tags"]:
                    print(f"  new tag {tag}")
            return 0
        result = repo.fetch(args.remote, on_progress=progress)
    finally:
        progress.done()
    print(f"fetched {result['objects']} objects from {args.remote}")
    for branch, commit in sorted(result["refs"].items()):
        print(f"  {args.remote}/{branch} {commit[:12] if commit else '(empty)'}")
    for tag in result["tags"]:
        print(f"  new tag {tag}")
    return 0


def _push_tags(repo: Repo, args: argparse.Namespace) -> int:
    """``push --tags`` and ``push <remote> <tag>`` (issue #40)."""
    if args.force_with_lease:
        raise RuntimeError("--force-with-lease does not apply to tags; --force replaces a tag on the remote")
    progress = Progress()
    try:
        result = repo.push_tags(
            args.remote,
            None if args.tags else [args.branch],
            on_progress=progress,
            force=args.force,
        )
    finally:
        progress.done()
    for tag, value in result["pushed"]:
        print(f"pushed tag {tag} {value[:12]} to {args.remote}")
    if result["pushed"]:
        print(f"copied {result['objects']} objects")
    else:
        print(f"tags are up to date on {args.remote}")
    return 0


def cmd_push(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.tags:
        if args.branch:
            raise RuntimeError("--tags pushes every tag; it cannot be combined with a branch or tag name")
        return _push_tags(repo, args)
    if repo.is_tag_push(args.branch):
        return _push_tags(repo, args)
    progress = Progress()
    try:
        result = repo.push(
            args.remote,
            args.branch,
            on_progress=progress,
            force=args.force,
            force_with_lease=args.force_with_lease,
        )
    finally:
        progress.done()
    forced = " (forced update)" if result.get("forced") else ""
    print(f"pushed {result['branch']} {result['commit'][:12]} to {args.remote}{forced}")
    print(f"copied {result['objects']} objects")
    return 0


@_releases_locks
def cmd_pull(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    progress = Progress()
    backups_before = len(repo.read_backups())
    try:
        result = repo.pull(args.remote, args.branch, on_progress=progress,
                           fetched=_PULL_FETCHED == args.remote)
    finally:
        progress.done()
    print(f"pull {result['mode']}")
    if result.get("commit"):
        print(f"HEAD is now {result['commit'][:12]}")
    _report_backups(repo, backups_before)
    if result.get("conflicts"):
        for path in result["conflicts"]:
            print(f"conflict {path}")
        print(f"conflict material written to {result['conflict_dir']}")
        _remind_reopen(repo.rewritten)
        return 1
    _remind_reopen(repo.rewritten)
    return 0


def cmd_clone(args: argparse.Namespace) -> int:
    from .transport import checked_refs, open_transport

    transport = open_transport(args.url)
    if args.directory:
        target = Path(args.directory).resolve()
    else:
        leaf = Path(args.url).name or "repo"
        for ext in (".tmi", ".git"):
            if leaf.endswith(ext) and len(leaf) > len(ext):
                leaf = leaf[: -len(ext)]
                break
        target = (Path.cwd() / leaf).resolve()
    if target.exists():
        if not target.is_dir():
            raise RuntimeError(f"target exists and is not a directory: {target}")
        if any(target.iterdir()):
            raise RuntimeError(f"target directory is not empty: {target}")
    target.mkdir(parents=True, exist_ok=True)
    repo = Repo.create(target)
    repo.add_remote("origin", args.url)
    progress = Progress()
    try:
        copied = transport.pull_objects(repo.store, on_progress=progress)
    finally:
        progress.done()
    all_refs = checked_refs(transport.list_refs())
    head_refs = {
        ref.removeprefix("refs/heads/"): oid
        for ref, oid in all_refs.items()
        if ref.startswith("refs/heads/")
    }
    for branch, commit in head_refs.items():
        if commit:
            repo.write_ref(f"refs/remotes/origin/{branch}", commit)
    repo.take_remote_tags(all_refs)
    default_branch = (
        transport.head_branch()
        or (DEFAULT_BRANCH if DEFAULT_BRANCH in head_refs else next(iter(head_refs), DEFAULT_BRANCH))
    )
    commit = head_refs.get(default_branch)
    if commit:
        repo.write_ref(f"refs/heads/{default_branch}", commit)
        repo.set_head_ref(f"refs/heads/{default_branch}")
        repo.restore_all(commit, reason="clone")
    stored_url = repo.remotes().get("origin", {}).get("url", args.url)
    print(f"cloned {stored_url} into {target}")
    print(f"copied {copied} objects")
    return 0


@_releases_locks
def cmd_merge(args: argparse.Namespace) -> int:
    repo = Repo.open()
    repo.lock_index()  # held until this command ends (issue #35)
    if args.abort:
        backups_before = len(repo.read_backups())
        try:
            _with_progress("aborting merge", repo.merge_abort)
        except Exception:
            _report_backups(repo, backups_before)
            raise
        print("merge aborted; working tree is back at HEAD")
        _report_backups(repo, backups_before)
        _remind_reopen(repo.rewritten)
        return 0
    if not args.rev:
        raise RuntimeError("merge requires a revision (or --abort)")
    progress = Progress()
    backups_before = len(repo.read_backups())
    try:
        result = repo.merge(args.rev, args.message, on_progress=progress)
    finally:
        progress.done()
    _report_backups(repo, backups_before)
    mode = result["mode"]
    if mode == "already-up-to-date":
        print("Already up to date.")
    elif mode == "fast-forward":
        print(f"Fast-forward to {result['commit'][:12]}")
    elif mode == "merge":
        print(f"Merged {args.rev}; HEAD is now {result['commit'][:12]}")
    if result.get("conflicts"):
        print(f"Automatic merge of {args.rev} stopped on conflicts:")
        for path in result["conflicts"]:
            print(f"  conflict {_textsafe.escape(path)}")
            for item in result.get("conflict_layers", {}).get(path, []):
                layer = item.get("path_name") or item.get("name")
                print(f"    {layer}: {item['reason']}" if layer else f"    {item['reason']}")
        # A format without layers (GIF, TIFF, ...), or a file added / deleted /
        # converted on one side, conflicts as a whole file: no <file>:<layer>
        # advice for it (issue #43).
        layered = any(
            item.get("stable_source_id")
            for items in result.get("conflict_layers", {}).values()
            for item in items
        )
        if layered:
            print("Layers that did not conflict are already merged into the working files;")
            print("each conflicting layer still holds our version. To resolve:")
            print("  tmiki restore --theirs <file>:<layer>   take their version of one layer")
        else:
            print("Each conflicting file still holds our version. To resolve:")
        print("  tmiki restore --ours|--theirs <file>    take one side of the whole file")
        print("  (or edit the file in your drawing app), then 'tmiki add <file>' and 'tmiki commit'")
        print("  tmiki merge --abort                     give up the merge")
        print(f"Both sides of each conflicting {'layer' if layered else 'file'}: {result['conflict_dir']}")
        _remind_reopen(repo.rewritten)
        return 1
    _remind_reopen(repo.rewritten)
    return 0


def _remind_reopen(paths: list[Path]) -> None:
    """tmi rewrites working files under the drawing app's feet; saving from a
    still-open document would silently undo that."""
    shown = [p for p in paths if p][:5]
    if shown:
        print("If any of these files is open in your drawing app, reopen it before saving:")
        for p in shown:
            print(f"  {p}")


@_releases_locks
def cmd_stash(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if (args.stash_command or "list") != "list":
        repo.lock_index()  # held until this command ends (issue #35)
    try:
        return _stash(repo, args)
    finally:
        _remind_reopen(repo.rewritten)


def _stash(repo: Repo, args: argparse.Namespace) -> int:
    # As in Git, `tmiki stash` alone is `tmiki stash push` (issue #39).
    command = args.stash_command or "push"
    if command in {"push", "save"}:
        message = getattr(args, "message", None)
        words = getattr(args, "message_words", None) or []
        if not message and words:
            message = " ".join(words)
        progress = Progress()
        try:
            stash = repo.stash_push(message, on_progress=progress)
        finally:
            progress.done()
        print(f"Saved working directory and index state stash@{{0}}: {stash['message']}")
        return 0
    if command == "list":
        for idx, stash in enumerate(repo.read_stashes()):
            print(f"stash@{{{idx}}}: {stash.get('message', '')} ({str(stash.get('commit', ''))[:12]})")
        return 0
    if command == "apply":
        stash = _with_progress("applying stash", lambda: repo.stash_apply(args.spec))
        print(f"applied {stash.get('commit', '')[:12]}")
        return 0
    if command == "pop":
        stash = _with_progress("applying stash", lambda: repo.stash_apply(args.spec))
        dropped = repo.stash_drop(args.spec)
        print(f"applied and dropped {dropped.get('commit', stash.get('commit', ''))[:12]}")
        return 0
    if command == "drop":
        dropped = repo.stash_drop(args.spec)
        print(f"dropped {dropped.get('commit', '')[:12]}")
        return 0
    if command == "clear":
        count = repo.stash_clear()
        print(f"dropped {count} stash entries")
        return 0
    raise RuntimeError(f"unknown stash command: {command}")


def cmd_hook_list(args: argparse.Namespace) -> int:
    repo = Repo.open()
    installed = set(_hooks.list_installed(repo.repo_dir))
    for name in _hooks.SUPPORTED_HOOKS:
        marker = "*" if name in installed else " "
        path = _hooks.hook_path(repo.repo_dir, name)
        print(f"{marker} {name:14} {path if name in installed else '(none)'}")
    return 0


def cmd_hook_run(args: argparse.Namespace) -> int:
    repo = Repo.open()
    code = _hooks.run(repo.repo_dir, args.name, fatal=False)
    print(f"hook {args.name} exited with {code}")
    return code


def cmd_gc(args: argparse.Namespace) -> int:
    repo = Repo.open()
    age = _parse_prune_age(args.prune)
    result = _with_progress("collecting garbage", lambda: repo.gc(prune_age_days=age))
    print(f"gc: kept {result['kept']} objects, removed {result['removed']}")
    if result.get("scratch_bytes_removed"):
        print(f"gc: removed {result['scratch_bytes_removed'] / 1e6:.1f} MB of diff reports, caches and temporary files")
    if result["reflog_entries_removed"]:
        print(f"gc: pruned {result['reflog_entries_removed']} reflog entries older than {args.prune}")
    return 0


def _parse_prune_age(spec: str) -> int:
    spec = spec.strip().lower()
    if spec == "now":
        return 0
    days: int | None = None
    if spec.endswith("d"):
        try:
            days = int(spec[:-1])
        except ValueError:
            days = None
    elif spec.isdigit():
        days = int(spec)
    if days is None or days < 0:
        raise RuntimeError(f"--prune expects 'now' or non-negative '<N>d' (got {spec!r})")
    return days


def cmd_recover(args: argparse.Namespace) -> int:
    repo = Repo.open()
    if args.backup is not None:
        backup, out = repo.restore_backup(args.backup, args.output)
        print(f"restored {backup['path']} ({backup['reason']}, {backup['timestamp'][:19].replace('T', ' ')}) to {out}")
        _remind_reopen(repo.rewritten)
        return 0
    if args.files:
        backups = repo.read_backups()
        if not backups:
            print("no backed-up files")
            return 0
        for idx, backup in enumerate(backups):
            when = str(backup.get("timestamp", ""))[:19].replace("T", " ")
            size = (backup.get("size") or 0) / 1e6
            print(f"{idx:3}  {when}  {backup.get('path')}  {size:.1f} MB  ({backup.get('reason')})")
        return 0
    if args.as_branch:
        if not args.rev:
            raise RuntimeError("--as requires a revision (HEAD@{N}, oid prefix, or branch name)")
        oid = _resolve_recover_rev(repo, args.rev)
        if not oid:
            raise RuntimeError(f"could not resolve revision: {args.rev}")
        repo.create_branch(args.as_branch, oid)
        print(f"created branch {args.as_branch} at {oid[:12]}")
        return 0

    entries = _reflog.read(repo.repo_dir, "HEAD")
    if not entries:
        print("no reflog yet")
        return 0
    for idx, entry in enumerate(reversed(entries)):
        ts = entry.timestamp.replace("T", " ").rstrip("Z")
        print(f"HEAD@{{{idx}}}  {entry.new_oid[:12]}  {ts}  {entry.reason}")
    return 0


def _resolve_recover_rev(repo: Repo, rev: str) -> str | None:
    at = _reflog.resolve_at_spec(repo.repo_dir, rev)
    if at:
        return at
    try:
        via_ref = repo.resolve_rev(rev)
    except RuntimeError:
        via_ref = None
    if via_ref:
        return via_ref
    if len(rev) >= 4 and all(c in "0123456789abcdef" for c in rev.lower()):
        for entry in _reflog.read(repo.repo_dir, "HEAD"):
            for oid in (entry.old_oid, entry.new_oid):
                if oid.startswith(rev.lower()) and oid != _reflog.ZERO_OID:
                    return oid
    return None


def cmd_config(args: argparse.Namespace) -> int:
    if args.list_all:
        repo_cfg: dict = {}
        try:
            repo = Repo.open()
        except RuntimeError:
            repo = None
        if repo is not None:
            repo_cfg = repo.read_config()
        user_cfg = _config.read_user_config()
        merged: dict[str, tuple[str, object]] = {}
        for key, value in _config.iter_dotted_keys(user_cfg):
            merged[key] = ("global", value)
        for key, value in _config.iter_dotted_keys(repo_cfg):
            merged[key] = ("local", value)
        for key in sorted(merged):
            scope, value = merged[key]
            print(f"{scope}\t{key}={value}")
        return 0

    if not args.key:
        raise RuntimeError("config requires a key (or --list)")

    if args.global_scope:
        data = _config.read_user_config()
        writer = _config.write_user_config
    else:
        repo = Repo.open()
        data = repo.read_config()
        writer = repo.write_config

    if args.unset:
        removed = _config.unset_value(data, args.key)
        if not removed:
            raise RuntimeError(f"key not set: {args.key}")
        writer(data)
        return 0

    if args.value is None:
        if args.key == _pixel_limit.CONFIG_KEY and not args.global_scope:
            # Effective value, wherever it comes from (env, repo or user
            # config, or the memory-based default).
            print(_pixel_limit.describe(repo.repo_dir))
            return 0
        value = _config.get_value(data, args.key)
        if value is None:
            return 1
        print(value)
        return 0

    value = args.value
    if args.key == _pixel_limit.CONFIG_KEY:
        value = str(_pixel_limit.parse_value(value))
    _config.set_value(data, args.key, value)
    writer(data)
    return 0


def first_line(value: str) -> str:
    lines = value.splitlines()
    return lines[0] if lines else ""
