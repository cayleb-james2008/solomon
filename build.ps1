# Build Solomon.exe (onedir) with PyInstaller, using the maki venv python
# (which already has pywebview + pyinstaller).
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$py = "C:\Users\Cayleb\Desktop\workspace\projects\maki\.venv\Scripts\python.exe"
# Run from this folder so dist\ and build\ land here (PyInstaller resolves them
# against the working dir, while spec-relative data paths stay anchored to the spec).
Set-Location $root
Write-Host "Building Solomon with PyInstaller..."
& $py -m PyInstaller --clean --noconfirm --distpath (Join-Path $root "dist") --workpath (Join-Path $root "build") (Join-Path $root "solomon.spec")
# $ErrorActionPreference="Stop" does NOT abort on a native-exe non-zero exit, so check explicitly —
# otherwise the script falls through to the else and returns exit 0 (false success) on a failed build.
if ($LASTEXITCODE -ne 0) { Write-Host "PyInstaller failed (exit $LASTEXITCODE)" -ForegroundColor Red; exit 1 }
$exe = Join-Path $root "dist\Solomon\Solomon.exe"
if (Test-Path $exe) { Write-Host "OK -> $exe" } else { Write-Host "BUILD FAILED" -ForegroundColor Red; exit 1 }
