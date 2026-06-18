# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller spec for Solomon (onedir). Bundles ONLY the web UI folder; the operator data
(repos.json, improver/, runtime/, .env) is NOT bundled — it lives in the real Solomon folder and is
resolved at runtime by control._base_dir(). Mirrors maki.spec's webview collect_all + clr pattern."""
from PyInstaller.utils.hooks import collect_all

# Only the web UI is bundled. The operator data (repos.json, improver/, runtime/, .env)
# lives in the real Solomon folder and is resolved at runtime by control._base_dir().
datas = [("web", "web")]
binaries = []
hiddenimports = [
    "control",
    "solomon",  # improver/solomon.py — the supervisor module, imported in-process by app.py
    "clr",  # pythonnet, used by pywebview edgechromium backend
]

# Collect webview (native libs / data / dynamic submodules).
for pkg in ("webview",):
    d, b, h = collect_all(pkg)
    datas += d
    binaries += b
    hiddenimports += h

a = Analysis(
    ["app.py"],
    pathex=["improver"],   # so PyInstaller can resolve `import solomon` (improver/solomon.py)
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    runtime_hooks=[],
    excludes=["tkinter", "pytest"],
    noarchive=False,
)
pyz = PYZ(a.pure)

exe = EXE(
    pyz, a.scripts, [], exclude_binaries=True,
    name="Solomon", console=False, disable_windowed_traceback=False,
    icon="web/assets/icon.ico" if __import__("os").path.exists("web/assets/icon.ico") else None,
)
coll = COLLECT(exe, a.binaries, a.datas, strip=False, upx=False, name="Solomon")
