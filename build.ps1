# Build Solomon.exe (onedir) with PyInstaller, using the maki venv python
# (which already has pywebview + pyinstaller).
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$py = "C:\Users\Cayleb\Desktop\workspace\projects\maki\.venv\Scripts\python.exe"
# Run from this folder so dist\ and build\ land here (PyInstaller resolves them
# against the working dir, while spec-relative data paths stay anchored to the spec).
Set-Location $root
# Guard: never build into a dist a live Solomon.exe occupies. A running exe holds its _internal
# files open, so PyInstaller's COLLECT silently skips the locked files and ships a half-built
# supervisor (same locked-dist failure class as Asmodeus.exe / the schema FileNotFoundError). Stop
# Solomon first. The in-app updater must stop the old exe before invoking this build.
$distApp = Join-Path $root "dist\Solomon"
$live = @(Get-CimInstance Win32_Process -Filter "Name='Solomon.exe'" -ErrorAction SilentlyContinue | Where-Object {
    (-not $_.ExecutablePath) -or $_.ExecutablePath.StartsWith($distApp, [System.StringComparison]::OrdinalIgnoreCase)
})
if ($live) {
    $ids = ($live | ForEach-Object { $_.ProcessId }) -join ', '
    [Console]::Error.WriteLine("BUILD ABORT: Solomon.exe is running (pids: $ids) from $distApp. A live exe locks its _internal files, so the rebuild would silently ship a half-built supervisor. Stop it first:  taskkill /F /IM Solomon.exe")
    exit 1
}
Write-Host "Building Solomon with PyInstaller..."
& $py -m PyInstaller --clean --noconfirm --distpath (Join-Path $root "dist") --workpath (Join-Path $root "build") (Join-Path $root "solomon.spec")
# $ErrorActionPreference="Stop" does NOT abort on a native-exe non-zero exit, so check explicitly —
# otherwise the script falls through to the else and returns exit 0 (false success) on a failed build.
if ($LASTEXITCODE -ne 0) { Write-Host "PyInstaller failed (exit $LASTEXITCODE)" -ForegroundColor Red; exit 1 }
# Completeness gate: never ship a half-built supervisor. Beyond the exe, the bundled web UI must be
# present (a COLLECT that silently dropped it would otherwise pass the old exe-only check).
$exe = Join-Path $root "dist\Solomon\Solomon.exe"
$web = Join-Path $root "dist\Solomon\_internal\web\index.html"
$missing = @(@($exe, $web) | Where-Object { -not (Test-Path -LiteralPath $_) })
if ($missing) {
    [Console]::Error.WriteLine("BUILD ABORT: built dist incomplete -- missing: $($missing -join '; '). A file was likely locked during COLLECT. Do NOT ship this dist.")
    exit 1
}
Write-Host "OK -> $exe (completeness gate passed: exe + web UI present)"
