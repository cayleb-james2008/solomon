@echo off
REM Solomon overnight watchdog — restarts crashed RSI loops, runs the supervisor RUNG-0 sweep,
REM and snapshots data to runtime\_monitor.jsonl. Invoked by the SolomonWatchdog scheduled task.
REM PYTHONPATH/PYTHONHOME are cleared so the maki venv python (3.12) uses its own stdlib.
setlocal
set "PYTHONPATH="
set "PYTHONHOME="
set "PY=C:\Users\Cayleb\Desktop\workspace\projects\maki\.venv\Scripts\python.exe"
set "MON=C:\Users\Cayleb\Desktop\workspace\solomon\monitor.py"
set "LOG=C:\Users\Cayleb\Desktop\workspace\solomon\runtime\_watchdog.out.log"
"%PY%" "%MON%" >> "%LOG%" 2>&1
endlocal
