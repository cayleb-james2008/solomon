"""No-console subprocess spawning for Windows GUI apps.

A console-subsystem child (python, node, pm2, gh, cmd, taskkill, ...) launched
from a GUI process gets a fresh *visible* console window unless suppressed.
``hidden_subprocess_kwargs()`` returns the kwargs that prevent that:
``CREATE_NO_WINDOW`` covers direct console children; ``STARTF_USESHOWWINDOW`` +
``SW_HIDE`` also suppress ``.cmd``/``.bat`` shim launchers (pm2.cmd, npm.cmd, gh).
Off Windows it returns ``{}`` so call sites stay cross-platform.

Pass the result with ``**``::

    subprocess.run(["gh", "pr", "list"], **hidden_subprocess_kwargs())
    subprocess.Popen(argv, **hidden_subprocess_kwargs(detached=True, new_group=True))

``detached`` (DETACHED_PROCESS) is for fire-and-forget children that must outlive
the parent; do NOT use it for a child you keep a handle on and ``poll()`` /
``terminate()`` (DETACHED breaks that on Windows). ``new_group``
(CREATE_NEW_PROCESS_GROUP) isolates the child from the parent's Ctrl-C / console
control signals.
"""

from __future__ import annotations

import subprocess
import sys
from typing import Any

_CREATE_NO_WINDOW = 0x08000000
_DETACHED_PROCESS = 0x00000008
_CREATE_NEW_PROCESS_GROUP = 0x00000200
_CREATE_NEW_CONSOLE = 0x00000010


def hidden_subprocess_kwargs(
    *, detached: bool = False, new_group: bool = False
) -> dict[str, Any]:
    """Windows kwargs that prevent a child console window. No-op off Windows."""
    if sys.platform != "win32":
        return {}
    flags = _CREATE_NO_WINDOW
    if detached:
        flags |= _DETACHED_PROCESS
    if new_group:
        flags |= _CREATE_NEW_PROCESS_GROUP
    startupinfo = subprocess.STARTUPINFO()
    startupinfo.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startupinfo.wShowWindow = 0  # SW_HIDE
    return {"creationflags": flags, "startupinfo": startupinfo}


def visible_console_kwargs() -> dict[str, Any]:
    """Windows kwargs giving a child its OWN VISIBLE console window — for an INTERACTIVE child the
    operator must see and respond to (e.g. ``gh auth login --web`` prints a one-time device code and
    waits for the operator to authorize in the browser). Do NOT redirect the child's std streams when
    using this, or the prompt/code is swallowed. No-op off Windows (the child inherits the terminal)."""
    if sys.platform != "win32":
        return {}
    return {"creationflags": _CREATE_NEW_CONSOLE}


__all__ = ["hidden_subprocess_kwargs", "visible_console_kwargs"]
