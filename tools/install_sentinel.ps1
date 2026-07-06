<#
.SYNOPSIS
Install / remove / query the Solomon Sentinel scheduled task.

.DESCRIPTION
The Sentinel is the out-of-band liveness layer (failure catalog #4: GUI-tick-only liveness caused
the 8h/25.5h watchdog gaps and the 2+ day outages; the "no scheduled tasks" rule was renegotiated
by the operator 2026-07-06 after the liveness autopsy). It runs `solomon.exe watchdog` every
5 minutes as the current user (default run level), host-independent of the visibly-open GUI.

No start-in directory is needed: paths::here() in the exe walks up to 5 ancestors from the exe dir
to the directory containing improver\, so the task resolves the operator data dir correctly from
src-tauri\target\release\ or the repo root alike. Each run stamps runtime\_sentinel_heartbeat.json
(dead-man visibility) and appends to runtime\_watchdog.out.log.

.EXAMPLE
powershell -File tools\install_sentinel.ps1            # install (or refresh) the task
powershell -File tools\install_sentinel.ps1 -Uninstall # delete the task
powershell -File tools\install_sentinel.ps1 -Status    # query the task
#>
param(
    [switch]$Uninstall,
    [switch]$Status
)

$TaskName = "Solomon Sentinel"

if ($Status) {
    schtasks /Query /TN "$TaskName" /V /FO LIST
    exit $LASTEXITCODE
}

if ($Uninstall) {
    schtasks /Delete /F /TN "$TaskName"
    exit $LASTEXITCODE
}

# Resolve the exe: the release build first, the repo-root production copy as fallback.
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "src-tauri\target\release\solomon.exe"
if (-not (Test-Path $exe)) {
    $exe = Join-Path $root "Solomon.exe"
}
if (-not (Test-Path $exe)) {
    Write-Error "no solomon.exe found (looked in src-tauri\target\release\ and the repo root) - build with 'cargo build --release' in src-tauri\ first"
    exit 1
}

# Current user, default (non-elevated) run level; /F replaces an existing task in place.
schtasks /Create /F /TN "$TaskName" /SC MINUTE /MO 5 /TR "`"$exe`" watchdog"
if ($LASTEXITCODE -eq 0) {
    Write-Output "installed: '$TaskName' -> `"$exe`" watchdog (every 5 minutes, current user)"
    Write-Output "verify:    schtasks /Query /TN `"$TaskName`""
    Write-Output "smoke:     schtasks /Run /TN `"$TaskName`"  then check runtime\_sentinel_heartbeat.json + runtime\_watchdog.out.log"
}
exit $LASTEXITCODE
