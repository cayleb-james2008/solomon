"""Solomon — a pywebview dashboard for cross-repo pi-powered auto-iterators.

Shows which improvers are running across registered repos (heartbeat + lock),
lets the operator start/stop them, and accept/deny their pull requests via gh.
Repos come from repos.json (a registry, so future repos drop in without code).
"""
import json
import os
import sys
import webbrowser

# NOTE: pywebview is imported lazily inside main() — the module-level import would crash the
# headless control surface (--state/--start/--stop/--supervise, used by tests and the scheduled
# supervisor sweep) on any host without pywebview/WebView2 installed.
import control

sys.path.insert(0, os.path.join(control.HERE, "improver"))  # solomon.py lives beside the runner
try:
    import solomon  # noqa: E402
except Exception:  # noqa: BLE001 — supervisor is additive; the dashboard still works without it
    solomon = None


def resource_path(rel: str) -> str:
    base = getattr(sys, "_MEIPASS", os.path.dirname(os.path.abspath(__file__)))
    return os.path.join(base, rel)


# operator data dir (resolved by control._base_dir even when frozen) for the persisted theme +
# global settings (auto_push). New name is .solomon.json; the legacy .rsi-control.json is read once.
_STATE_FILE = os.path.join(control.HERE, ".solomon.json")
_LEGACY_STATE_FILE = os.path.join(control.HERE, ".rsi-control.json")


def _load_state() -> dict:
    """Persisted theme + global settings. One-time migration: only when .solomon.json is absent/unreadable
    do we fall back to the legacy .rsi-control.json (older builds wrote that name)."""
    try:
        with open(_STATE_FILE, "r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        pass
    try:                                    # legacy fallback ONLY when the current state file is missing
        with open(_LEGACY_STATE_FILE, "r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


_UNSET = object()   # sentinel: distinguishes "key was absent" from a real prior value on rollback


def _save_state(state: dict) -> bool:
    """Atomically persist state (tmp + os.replace — never truncate-in-place, matching control.py's
    repos.json/lock writers). A crash/concurrent write mid-dump must not leave a half-written
    .solomon.json that next launch reads as {} — which silently reverts the auto_push/auto_ai_fix
    safety dials to their permissive defaults. Returns True on success, False on OSError so the setters
    can tell the UI whether the dial actually reached disk (its rollback only fires on ok:false)."""
    tmp = _STATE_FILE + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(state, f, indent=2)
        os.replace(tmp, _STATE_FILE)
        return True
    except OSError:
        try:
            os.remove(tmp)          # don't leak the half-written tmp when the write/replace fails
        except OSError:
            pass
        return False


class Api:
    def __init__(self):
        self._state = _load_state()
        self._window = None          # set in main(); used by apply_update to close after spawn
        self._update = None          # last launch-time update_status() result (cached for the UI)

    # ---- in-app updater (source-rebuild) --------------------------------
    def current_sha(self):
        """Short git sha of Solomon's own checkout, shown as the version identity."""
        return control.current_sha()

    def update_status(self):
        """Live check: is this Solomon checkout behind origin? Caches the result for the UI."""
        self._update = control.update_status()
        return self._update

    def cached_update_status(self):
        """The launch-time check result (or None if it hasn't run yet) — no network call."""
        return self._update

    def _bg_update_check(self):
        """Launch-time check, run in a background thread so the window opens instantly."""
        try:
            self._update = control.update_status()
        except Exception:  # noqa: BLE001 — a check failure must never crash startup
            self._update = {"ok": False, "available": False}

    def apply_update(self):
        """Spawn the source-rebuild updater, then close the window so it can rebuild + relaunch."""
        res = control.apply_update()
        if res.get("started") and self._window is not None:
            # Give the bridge call time to return to the UI before the window tears down.
            import threading
            threading.Timer(0.6, self._window.destroy).start()
        return res

    # ---- combined dashboard state ---------------------------------------
    def get_state(self):
        repos = control.load_repos()
        gh_ready = control.gh_ready()

        def _safe(fn, default):
            # PER-FIELD resilience: a single flaky git/gh call (e.g. branch_hygiene/list_worktrees racing
            # a loop's in-flight git op, an index.lock contention, a hung gh) must degrade ONLY that field
            # — never throw out of the whole card to a fallback that lies running:False and blinds the
            # operator about real loop state. fn is invoked immediately (no late-binding over the loop var).
            try:
                return fn()
            except Exception:  # noqa: BLE001
                return default

        out = []
        for r in repos:
            if not isinstance(r, dict):
                continue
            out.append({
                "name": r.get("name"),
                "path": r.get("path"),
                "provider": _safe(lambda: control.project_provider(r), "ollama-cloud"),
                "model": _safe(lambda: control.project_model(r), None),
                "ship": _safe(lambda: control.project_ship(r), "pr"),
                "gate": _safe(lambda: control.project_gate(r), None),
                "pr_target_branch": _safe(lambda: control.project_pr_target_branch(r), "main"),
                "reasoning": _safe(lambda: control.project_reasoning(r), ""),
                "goal": _safe(lambda: control.project_goal(r), ""),
                "interval": _safe(lambda: control.project_interval(r), 120),
                "max_iterations": _safe(lambda: control.project_max_iterations(r), 0),
                "phases": r.get("phases") or {},
                "is_git": bool(r.get("is_git")),
                "has_remote": bool(r.get("has_remote")),
                "running": _safe(lambda: control.is_running(r), False),
                "heartbeat": _safe(lambda: control.read_heartbeat(r), None),
                "prs": (_safe(lambda: control.list_prs(r), []) if gh_ready else []),
                "local_branches": _safe(lambda: control.local_rsi_branches(r), []),
                "worktrees": _safe(lambda: control.list_worktrees(r), []),
                "hygiene": _safe(lambda: control.branch_hygiene(r), {"dirty": False}),
                "frontend": _safe(lambda: control.has_frontend(r), False),
                "browser": _safe(lambda: control.browser_state(r), {"ok": False}),
                "contracts": _safe(lambda: control.contracts_present(r), {"agent": False, "backlog": False}),
                "diagnosis": (_safe(lambda: solomon.diagnose(r), {"category": "ok", "healthy": True})
                              if solomon else {"category": "ok", "healthy": True}),
                "escalation": (_safe(lambda: solomon.read_escalation(r), None) if solomon else None),
            })
        return {"repos": out, "gh_ready": gh_ready, "theme": self.get_theme(),
                "auto_push": self.get_auto_push(), "auto_ai_fix": self.get_auto_ai_fix(),
                "providers": ["ollama-cloud", "openrouter"],
                "keys": control.keys_status(), "github": control.github_status()}

    def _repo(self, name):
        for r in control.load_repos():
            if r.get("name") == name:
                return r
        return None

    # ---- control --------------------------------------------------------
    def start(self, name, once=False):
        r = self._repo(name)
        return control.start(r, auto_push=self.get_auto_push(), once=once) if r \
            else {"ok": False, "error": "unknown repo"}

    def stop(self, name):
        r = self._repo(name)
        return control.stop(r) if r else {"ok": False, "error": "unknown repo"}

    def beautify(self, name):
        r = self._repo(name)
        return control.beautify(r, auto_push=self.get_auto_push()) if r \
            else {"ok": False, "error": "unknown repo"}

    def merge(self, name, number):
        r = self._repo(name)
        return control.merge_pr(r, number) if r else {"ok": False, "error": "unknown repo"}

    def close(self, name, number):
        r = self._repo(name)
        return control.close_pr(r, number) if r else {"ok": False, "error": "unknown repo"}

    # ---- config ---------------------------------------------------------
    def set_repo_config(self, name, provider=None, model=None, ship=None, gate=None,
                        pr_target_branch=None, interval=None, max_iterations=None,
                        reasoning=None, goal=None, phases=None):
        return control.set_repo_config(name, provider=provider, model=model, ship=ship, gate=gate,
                                       pr_target_branch=pr_target_branch, interval=interval,
                                       max_iterations=max_iterations, reasoning=reasoning, goal=goal,
                                       phases=phases)

    def set_key(self, provider, value):
        return control.set_key(provider, value)

    def add_project(self, spec, goal=None):
        r = control.add_project(spec)
        if r.get("ok"):                       # auto-enrich the new repo's contract once, in the background
            if goal:                          # set the north-star goal FIRST so the enrichment is steered by it
                control.set_repo_config(r.get("name"), goal=goal.strip())
            repo = self._repo(r.get("name"))
            if repo and control.enrich_contract(repo, background=True).get("ok"):
                r = {**r, "enriching": True}
        return r

    def connect_project(self, spec, goal=None, ship="pr", visual_gate=None, provider=None):
        return control.connect_project(spec, goal=goal, ship=ship, visual_gate=visual_gate,
                                       provider=provider)

    def github_login_start(self):
        return control.github_login_start()

    # ---- review / workspace / insights ----------------------------------
    def pr_diff(self, name, number):
        r = self._repo(name)
        return control.pr_diff(r, number) if r else {"ok": False, "error": "unknown repo"}

    def read_log(self, name):
        r = self._repo(name)
        return control.read_log(r) if r else {"ok": False, "error": "unknown repo"}

    def read_history(self, name):
        r = self._repo(name)
        return control.read_history(r) if r else []

    def read_contract(self, name, which):
        r = self._repo(name)
        return control.read_contract(r, which) if r else {"ok": False, "error": "unknown repo"}

    def write_contract(self, name, which, text):
        r = self._repo(name)
        return control.write_contract(r, which, text) if r else {"ok": False, "error": "unknown repo"}

    def metrics(self, name):
        r = self._repo(name)
        return control.metrics(r) if r else {}

    def cleanup_worktrees(self, name):
        r = self._repo(name)
        return control.cleanup_worktrees(r) if r else {"ok": False, "error": "unknown repo"}

    def clean_branch(self, name):
        r = self._repo(name)
        return control.clean_branch(r) if r else {"ok": False, "error": "unknown repo"}

    def start_app_test(self, name):
        r = self._repo(name)
        return control.start_app_test(r) if r else {"ok": False, "error": "unknown repo"}

    def stop_app_test(self, name):
        r = self._repo(name)
        return control.stop_app_test(r) if r else {"ok": False, "error": "unknown repo"}

    def app_test_state(self, name, after_seq=0):
        r = self._repo(name)
        return control.app_test_state(r, after_seq=after_seq) if r else {
            "ok": False, "error": "unknown repo"}

    def app_test_frame(self, name, after_seq=0):
        r = self._repo(name)
        return control.app_test_frame(r, after_seq=after_seq) if r else {
            "ok": False, "error": "unknown repo"}

    def read_app_test_report(self, name):
        r = self._repo(name)
        return control.read_app_test_report(r) if r else {"ok": False, "error": "unknown repo"}

    # ---- provisioning + Solomon supervisor ------------------------------
    def ensure_contracts(self, name):
        r = self._repo(name)
        return control.ensure_contracts(r) if r else {"ok": False, "error": "unknown repo"}

    def enrich_contract(self, name):
        r = self._repo(name)
        return control.enrich_contract(r) if r else {"ok": False, "error": "unknown repo"}

    def ideate(self, name):
        r = self._repo(name)
        return control.ideate(r) if r else {"ok": False, "error": "unknown repo"}

    def supervise(self, name=None, allow_pi=False, unattended=False):
        """Diagnose + recover one repo (name) or all (name=None). RUNG-0 deterministic recovery runs
        immediately. A pi fix-session runs ONLY when explicitly allowed: allow_pi (the per-action
        'Allow AI fix' tick from the UI button), OR — strictly on the unattended/automated sweep
        (unattended=True, i.e. `--supervise`) — the global auto_ai_fix setting. The interactive
        Supervise button NEVER auto-spawns a fix from the global setting alone. allow_restart follows
        the global auto-push gate."""
        if not solomon:
            return {"ok": False, "error": "supervisor unavailable"}
        targets = [r for r in control.load_repos()
                   if isinstance(r, dict) and (name is None or r.get("name") == name)]
        if name is not None and not targets:
            return {"ok": False, "error": "unknown repo"}
        allow = bool(allow_pi) or (unattended and self.get_auto_ai_fix())
        auto_push = self.get_auto_push()
        # Per-repo guard (like get_state / _health_payload): one repo whose recover() raises must NOT
        # abort the whole sweep — under `--supervise` that would exit non-zero with a traceback and leave
        # every later repo un-recovered. Surface the failure per-repo and keep going.
        results = []
        for r in targets:
            try:
                results.append({"name": r.get("name"),
                                **solomon.recover(r, allow_pi=allow, allow_restart=auto_push, auto_push=auto_push)})
            except Exception as e:  # noqa: BLE001 — one bad repo must not abort the sweep
                results.append({"name": r.get("name"), "ok": False, "error": str(e), "escalate": True})
        return {"ok": True, "results": results}

    def read_supervisor_log(self, name):
        r = self._repo(name)
        return control.read_supervisor_log(r) if r else []

    def read_escalation(self, name):
        r = self._repo(name)
        return solomon.read_escalation(r) if (r and solomon) else None

    def clear_escalation(self, name):
        r = self._repo(name)
        return solomon.clear_escalation(r) if (r and solomon) else {"ok": False, "error": "unknown repo"}

    # ---- misc -----------------------------------------------------------
    def open_url(self, url):
        # Only open real web links. os.startfile launches the shell association for ANY string
        # (local .exe, UNC \\host\share, file://), so gate on an http(s) scheme and use webbrowser.open
        # (itself scheme-limited) rather than os.startfile, to keep this bridge method narrow.
        if not isinstance(url, str) or not url.lower().startswith(("http://", "https://")):
            return {"ok": False, "error": "only http(s) URLs are allowed"}
        try:
            webbrowser.open(url)
            return {"ok": True}
        except OSError as e:
            return {"ok": False, "error": str(e)}

    def get_theme(self):
        return self._state.get("theme", "dark")

    def _set_state(self, key, value) -> bool:
        """Set self._state[key] and persist. On a FAILED persist, ROLL BACK the in-memory value so the
        getters (read by the supervisor — get_auto_ai_fix / get_auto_push) never report an un-persisted
        value that disk and the UI don't reflect. Returns whether the value reached disk."""
        prev = self._state.get(key, _UNSET)
        self._state[key] = value
        if _save_state(self._state):
            return True
        if prev is _UNSET:
            self._state.pop(key, None)
        else:
            self._state[key] = prev
        return False

    def set_theme(self, t):
        return {"ok": self._set_state("theme", t), "theme": t}

    def get_auto_push(self):
        """Global auto-push gate (default True). When False, runs ship 'local' (no push/PR)."""
        return bool(self._state.get("auto_push", True))

    def set_auto_push(self, v):
        return {"ok": self._set_state("auto_push", bool(v)), "auto_push": bool(v)}

    def get_auto_ai_fix(self):
        """Global opt-in: when True the supervisor may run a pi fix-session unattended on a
        gate-red streak (no per-action 'Allow AI fix' tick needed). Default False."""
        return bool(self._state.get("auto_ai_fix", False))

    def set_auto_ai_fix(self, v):
        return {"ok": self._set_state("auto_ai_fix", bool(v)), "auto_ai_fix": bool(v)}


def _health_payload() -> dict:
    """The JSON body the health endpoint serves: overall readiness + a per-repo diagnose summary.
    Pure (no socket) so it can be unit-tested directly."""
    repos = []
    for r in control.load_repos():
        if not isinstance(r, dict):
            continue
        name = r.get("name")
        if solomon:
            try:
                d = solomon.diagnose(r)
                repos.append({"name": name, "running": d.get("running"),
                              "healthy": d.get("healthy"), "category": d.get("category"),
                              "evidence": d.get("evidence")})
                continue
            except Exception as e:  # noqa: BLE001 — one bad repo must not blank the endpoint
                repos.append({"name": name, "category": "?", "error": str(e)})
                continue
        repos.append({"name": name, "running": control.is_running(r)})
    return {"ok": True, "health": control.health(), "repos": repos}


def serve_health(port: int = 8787):
    """Headless/plug-and-play monitoring: a tiny stdlib HTTP server on 127.0.0.1 that serves the
    health payload as JSON on GET /health (404 elsewhere). stdlib only — no web framework. Blocks."""
    import http.server

    class _H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802 — http.server's required method name
            if self.path.split("?", 1)[0] != "/health":
                self.send_error(404, "not found")
                return
            body = json.dumps(_health_payload()).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):  # silence the default stderr access log
            pass

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), _H)
    print(f"Solomon health endpoint: http://127.0.0.1:{httpd.server_address[1]}/health")
    httpd.serve_forever()


def _single_instance_or_foreground() -> bool:
    """Single-instance guard. Returns True if we are the sole dashboard (open the window). If another
    Solomon dashboard is already running, FOREGROUND its window and return False (this process exits) —
    so launching Solomon again (taskbar / open_application) re-focuses the one window instead of opening
    a DUPLICATE. Windows-only (ctypes, no new dep); a no-op (always True) elsewhere."""
    if sys.platform != "win32":
        return True
    try:
        import ctypes
        k32 = ctypes.windll.kernel32
        k32.CreateMutexW(None, False, "Global\\Solomon.Dashboard.SingleInstance")  # handle held until process exit
        if k32.GetLastError() != 183:        # 183 = ERROR_ALREADY_EXISTS -> we're the first instance
            return True
        user32 = ctypes.windll.user32        # another instance exists — bring its window to the front
        hwnd = user32.FindWindowW(None, "Solomon")
        if hwnd:
            user32.ShowWindow(hwnd, 9)       # SW_RESTORE (un-minimize)
            user32.SetForegroundWindow(hwnd)
        return False
    except Exception:  # noqa: BLE001 — a guard failure must never block opening the dashboard
        return True


def main():
    if sys.platform == "win32":
        try:  # distinct taskbar identity so Windows uses Solomon's icon (not python's) + groups/pins correctly
            import ctypes
            ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("Solomon.Dashboard")
        except Exception:  # noqa: BLE001 — best-effort cosmetic
            pass
    if not _single_instance_or_foreground():
        return                               # a dashboard is already open — we foregrounded it; don't duplicate
    import threading

    # Re-arm the keep-alive watchdog if its scheduled task was deleted/disabled (idempotent, best-
    # effort, never blocks startup) — so opening the dashboard restores crash-recovery for the loops.
    try:
        import control
        control._ensure_watchdog_task()
    except Exception:  # noqa: BLE001 — best-effort; a watchdog-arm failure must never block the GUI
        pass

    import webview  # lazy: GUI-only dependency, not needed by the headless control surface
    api = Api()
    api._window = webview.create_window(
        "Solomon",
        url=resource_path(os.path.join("web", "index.html")),
        js_api=api, width=1000, height=760, min_size=(820, 600),
        background_color="#191917",
    )
    # Launch-time update check, in a background thread so the window opens without waiting on git fetch.
    threading.Thread(target=api._bg_update_check, daemon=True).start()
    webview.start(gui="edgechromium")


if __name__ == "__main__":
    import multiprocessing
    multiprocessing.freeze_support()
    # headless control surface (so the exe is scriptable + testable without the GUI)
    if len(sys.argv) > 1 and sys.argv[1].startswith("--"):
        api = Api()
        cmd, arg = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else None)
        if cmd == "--state":
            print(json.dumps(api.get_state(), indent=2))
        elif cmd == "--start" and arg:
            print(json.dumps(api.start(arg)))
        elif cmd == "--stop" and arg:
            print(json.dumps(api.stop(arg)))
        elif cmd == "--supervise":
            # unattended sweep (one repo or all): RUNG-0 always; a pi fix only if the global AI-fixes
            # setting is on; restart only if auto_push. (The interactive Supervise button is RUNG-0 +
            # the explicit 'Allow AI fix' tick only — the global setting never auto-fires it.)
            print(json.dumps(api.supervise(arg, unattended=True), indent=2))
        elif cmd == "--serve-health":
            # headless operator: poll http://127.0.0.1:<port>/health for readiness + per-repo state.
            serve_health(int(arg) if arg and arg.isdigit() else 8787)
        else:
            print("usage: Solomon --state | --start <name> | --stop <name> | --supervise [name] "
                  "| --serve-health [port]")
        sys.exit(0)
    main()
