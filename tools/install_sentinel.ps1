<#
.SYNOPSIS
DEPRECATED — the Solomon Sentinel scheduled task violates the settled liveness doctrine.
Only -Uninstall and -Status still work (to remove/inspect a historical task); install refuses.

.DESCRIPTION
LIVENESS DOCTRINE (operator, SETTLED — supersedes the 2026-07-06 "sentinel" renegotiation this
script was born from): NO background processes and NO scheduled tasks, ever (no schtasks, no cron,
no daemons). The watchdog lives INSIDE the visibly-open Solomon.exe (the run_gui tick thread,
every 2 min, with a catch-up sweep the moment the app opens); Solomon.exe is launched manually by
the operator, never auto-started. See README.md ("no scheduled task exists and none may be
created — operator rule") and AGENTS.md ("Watchdog sweep").

This file is kept (rather than deleted) so the history of the 2026-07-06 renegotiation and the
uninstall/query paths survive: if a "Solomon Sentinel" task still exists on a machine from that
era, remove it with -Uninstall.

.EXAMPLE
powershell -File tools\install_sentinel.ps1 -Uninstall # delete a historical task (do this)
powershell -File tools\install_sentinel.ps1 -Status    # query whether one still exists
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

# DEPRECATED install path: the settled operator doctrine forbids scheduled tasks/background
# processes. The in-app watchdog tick (visible Solomon.exe) is the ONLY automatic sweep.
Write-Error "DEPRECATED: installing the Solomon Sentinel scheduled task is forbidden by the settled liveness doctrine (no background processes / no schtasks; the watchdog lives inside the visible Solomon.exe). Use -Uninstall to remove a historical task, -Status to query."
exit 1

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
# The 5-min cadence is the FLEET-PLANE max-park liveness FLOOR (audit A.1 / park.rs::MAX_PARK_FLOOR_S
# = 300s): the guaranteed upper bound on how long the fleet can go un-swept even with no in-app GUI
# tick. It is a floor, not the primary pacing — the per-lane improver loop now wakes event-driven
# (freshness new-sample / KILL, see improver::park) well inside this window. Do NOT lengthen it.
schtasks /Create /F /TN "$TaskName" /SC MINUTE /MO 5 /TR "`"$exe`" watchdog"
if ($LASTEXITCODE -eq 0) {
    Write-Output "installed: '$TaskName' -> `"$exe`" watchdog (every 5 minutes, current user)"
    Write-Output "verify:    schtasks /Query /TN `"$TaskName`""
    Write-Output "smoke:     schtasks /Run /TN `"$TaskName`"  then check runtime\_sentinel_heartbeat.json + runtime\_watchdog.out.log"
}
exit $LASTEXITCODE
