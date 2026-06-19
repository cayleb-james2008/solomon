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


def _save_state(state: dict) -> None:
    try:
        with open(_STATE_FILE, "w", encoding="utf-8") as f:
            json.dump(state, f, indent=2)
    except OSError:
        pass


class Api:
    def __init__(self):
        self._state = _load_state()

    # ---- combined dashboard state ---------------------------------------
    def get_state(self):
        repos = control.load_repos()
        gh_ready = control.gh_ready()
        out = []
        for r in repos:
            if not isinstance(r, dict):
                continue
            try:
                out.append({
                    "name": r.get("name"),
                    "path": r.get("path"),
                    "provider": control.project_provider(r),
                    "model": control.project_model(r),
                    "ship": control.project_ship(r),
                    "gate": control.project_gate(r),
                    "pr_target_branch": control.project_pr_target_branch(r),
                    "reasoning": control.project_reasoning(r),
                    "goal": control.project_goal(r),
                    "interval": control.project_interval(r),
                    "max_iterations": control.project_max_iterations(r),
                    "phases": r.get("phases") or {},
                    "is_git": bool(r.get("is_git")),
                    "has_remote": bool(r.get("has_remote")),
                    "running": control.is_running(r),
                    "heartbeat": control.read_heartbeat(r),
                    "prs": control.list_prs(r) if gh_ready else [],
                    "local_branches": control.local_rsi_branches(r),
                    "worktrees": control.list_worktrees(r),
                    "frontend": control.has_frontend(r),
                    "browser": control.browser_state(r),
                    "contracts": control.contracts_present(r),
                    "diagnosis": (solomon.diagnose(r) if solomon else {"category": "ok", "healthy": True}),
                    "escalation": (solomon.read_escalation(r) if solomon else None),
                })
            except Exception as e:  # noqa: BLE001 — one bad repo must not blank the dashboard
                out.append({"name": r.get("name") or "?", "path": r.get("path"),
                            "provider": "ollama-cloud", "model": None,
                            "ship": "pr", "gate": None, "pr_target_branch": "main",
                            "reasoning": "", "goal": "", "interval": 120, "max_iterations": 0,
                            "is_git": bool(r.get("is_git")), "has_remote": bool(r.get("has_remote")),
                            "running": False, "heartbeat": None, "prs": [], "local_branches": [],
                            "contracts": {"agent": False, "backlog": False},
                            "diagnosis": {"category": "ok", "healthy": True}, "escalation": None,
                            "error": str(e)})
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

    def connect_project(self, spec, goal=None, ship="pr", visual_gate=None):
        return control.connect_project(spec, goal=goal, ship=ship, visual_gate=visual_gate)

    def github_login_start(self):
        return control.github_login_start()

    def publish(self, name, private=True):
        return control.publish_to_github(name, private=private)

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

    def health(self):
        return control.health()

    def cleanup_worktrees(self, name):
        r = self._repo(name)
        return control.cleanup_worktrees(r) if r else {"ok": False, "error": "unknown repo"}

    def list_worktrees(self, name):
        r = self._repo(name)
        return control.list_worktrees(r) if r else []

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
        results = [{"name": r.get("name"),
                    **solomon.recover(r, allow_pi=allow, allow_restart=auto_push, auto_push=auto_push)}
                   for r in targets]
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

    def set_theme(self, t):
        self._state["theme"] = t
        _save_state(self._state)
        return {"ok": True, "theme": t}

    def get_auto_push(self):
        """Global auto-push gate (default True). When False, runs ship 'local' (no push/PR)."""
        return bool(self._state.get("auto_push", True))

    def set_auto_push(self, v):
        self._state["auto_push"] = bool(v)
        _save_state(self._state)
        return {"ok": True, "auto_push": bool(v)}

    def get_auto_ai_fix(self):
        """Global opt-in: when True the supervisor may run a pi fix-session unattended on a
        gate-red streak (no per-action 'Allow AI fix' tick needed). Default False."""
        return bool(self._state.get("auto_ai_fix", False))

    def set_auto_ai_fix(self, v):
        self._state["auto_ai_fix"] = bool(v)
        _save_state(self._state)
        return {"ok": True, "auto_ai_fix": bool(v)}


def main():
    if sys.platform == "win32":
        try:  # distinct taskbar identity so Windows uses Solomon's icon (not python's) + groups/pins correctly
            import ctypes
            ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("Solomon.Dashboard")
        except Exception:  # noqa: BLE001 — best-effort cosmetic
            pass
    import webview  # lazy: GUI-only dependency, not needed by the headless control surface
    api = Api()
    webview.create_window(
        "Solomon",
        url=resource_path(os.path.join("web", "index.html")),
        js_api=api, width=1000, height=760, min_size=(820, 600),
        background_color="#191917",
    )
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
        else:
            print("usage: Solomon --state | --start <name> | --stop <name> | --supervise [name]")
        sys.exit(0)
    main()
