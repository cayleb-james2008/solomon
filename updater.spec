# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller spec for the Solomon Updater — a standalone console exe that pulls the
latest from the solomon git repo, rebuilds Solomon.exe if needed, then launches it.

Build with (note --distpath dist/updater: PyInstaller names the onedir after the COLLECT
name 'SolomonUpdater', and control.apply_update looks for it at
dist/updater/SolomonUpdater/SolomonUpdater.exe):
  <maki-venv-python> -m PyInstaller --noconfirm --distpath dist/updater --workpath build updater.spec

The updater is a CONSOLE exe (console=True) so the operator sees the update/build progress.
It depends only on the Python stdlib (no pywebview, no third-party deps) so the build is fast
and the exe is small. It finds the solomon source repo by walking up from its own location
(or SOLOMON_HOME), so place it anywhere convenient — typically dist/Solomon/updater.exe or a
desktop shortcut pointing at it."""
import os

a = Analysis(
    ["updater.py"],
    pathex=[],
    binaries=[],
    datas=[],
    hiddenimports=[],
    hookspath=[],
    runtime_hooks=[],
    excludes=["tkinter", "pytest", "webview", "pywebview"],
    noarchive=False,
)
pyz = PYZ(a.pure)
exe = EXE(
    pyz, a.scripts, [],
    exclude_binaries=True,        # onedir: binaries go in the COLLECT dir, not the exe
    name="SolomonUpdater",
    console=True,                  # show the update/build progress in a console window
    disable_windowed_traceback=False,
    icon="web/assets/icon.ico" if os.path.exists("web/assets/icon.ico") else None,
)
coll = COLLECT(exe, a.binaries, a.datas, strip=False, upx=False, name="SolomonUpdater")