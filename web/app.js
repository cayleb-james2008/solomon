/* Solomon — glassmorphic control surface over the pywebview Api bridge.
   Customizable panel workspace: open panels from the bento menu, drag to rearrange.
   Append ?mock=1 to render with sample data in a plain browser (no backend). */

/* ---------- tiny DOM helpers ---------- */
const $ = (s, r = document) => r.querySelector(s);
const esc = (s) => (s == null ? "" : String(s).replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c])));
function h(tag, cls, html) { const e = document.createElement(tag); if (cls) e.className = cls; if (html != null) e.innerHTML = html; return e; }
let _uid = 0; const uid = () => "p" + (++_uid) + "_" + Date.now().toString(36);

/* ---------- constants ---------- */
const MOCK = /[?&]mock/.test(location.search);
const SHIP = [["local", "Local"], ["push", "Push"], ["pr", "PR"], ["auto-merge", "Auto-merge"]];
const REASON = ["off", "minimal", "low", "medium", "high", "xhigh"];
const PROV = { "ollama-cloud": "Ollama Cloud", "openrouter": "OpenRouter" };
const KNOWN_MODELS = ["kimi-k2.7-code", "glm-5.2", "minimax-m3", "nex-agi/nex-n2-pro:free", "qwen3-coder", "deepseek-v3"];
const PANEL_TYPES = {
  loops: { title: "Loop Controls", icon: '<rect x="3" y="4" width="18" height="6" rx="2"/><rect x="3" y="14" width="18" height="6" rx="2"/><circle cx="7" cy="7" r="1.2"/><circle cx="7" cy="17" r="1.2"/>', desc: "Start/Stop + selectors for every repo" },
  activity: { title: "Activity", icon: '<path d="M3 12h4l2 6 4-14 2 8h6"/>', desc: "Live log + recent iterations" },
  approvals: { title: "Approvals", icon: '<path d="M9 12l2 2 4-4"/><circle cx="12" cy="12" r="9"/>', desc: "Open pull requests across repos" },
};
const ICONS = {
  play: '<path d="M8 5l11 7-11 7z" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6.5" y="6.5" width="11" height="11" rx="1.5" fill="currentColor" stroke="none"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  expand: '<path d="M4 9V4h5M20 15v5h-5"/>',
  grip: '<circle cx="9" cy="6" r="1.3"/><circle cx="15" cy="6" r="1.3"/><circle cx="9" cy="12" r="1.3"/><circle cx="15" cy="12" r="1.3"/><circle cx="9" cy="18" r="1.3"/><circle cx="15" cy="18" r="1.3"/>',
  merge: '<circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="9" r="2.5"/><path d="M6 8.5v7M8.4 7.5A6 6 0 0 0 15.5 9.6"/>',
  github: '<path d="M12 2a10 10 0 0 0-3.2 19.5c.5.1.7-.2.7-.5v-2c-2.8.6-3.4-1.2-3.4-1.2-.5-1.2-1.1-1.5-1.1-1.5-.9-.6.1-.6.1-.6 1 .1 1.5 1 1.5 1 .9 1.5 2.3 1.1 2.9.8.1-.6.3-1.1.6-1.3-2.2-.300000000000004-4.5-1.1-4.5-5a3.9 3.9 0 0 1 1-2.7 3.6 3.6 0 0 1 .1-2.7s.8-.3 2.7 1a9.3 9.3 0 0 1 5 0c1.9-1.3 2.7-1 2.7-1 .5 1.4.2 2.4.1 2.7a3.9 3.9 0 0 1 1 2.7c0 3.9-2.3 4.7-4.5 5 .3.3.6.9.6 1.8v2.7c0 .3.2.6.7.5A10 10 0 0 0 12 2z"/>',
};
function ic(name, size = 16) { return `<svg viewBox="0 0 24 24" width="${size}" height="${size}" class="ic">${ICONS[name] || ""}</svg>`; }

/* ---------- bridge ---------- */
function realApi() { return (window.pywebview && window.pywebview.api) || null; }
async function call(m, ...a) {
  if (MOCK || !realApi()) { if (mock[m]) return mock[m](...a); throw new Error("no api: " + m); }
  const x = realApi();
  if (!x[m]) throw new Error("API method missing: " + m);
  return x[m](...a);
}
async function act(m, ...a) { try { return await call(m, ...a); } catch (e) { return { ok: false, error: (e && e.message) || String(e) }; } }

function toast(msg, type = "") {
  const t = h("div", "toast " + type, esc(msg)); $("#toasts").appendChild(t);
  setTimeout(() => { t.style.transition = ".3s"; t.style.opacity = "0"; setTimeout(() => t.remove(), 320); }, 3200);
}
function ago(iso) {
  if (!iso) return ""; const t = Date.parse(iso); if (isNaN(t)) return "";
  const s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return s + "s ago"; if (s < 3600) return Math.round(s / 60) + "m ago";
  if (s < 86400) return Math.round(s / 3600) + "h ago"; return Math.round(s / 86400) + "d ago";
}

/* ---------- app state ---------- */
const state = { repos: [], providers: ["ollama-cloud", "openrouter"], gh_ready: false, keys: {}, github: {}, auto_push: true };
let layout = [];
const panels = new Map();   // id -> { el, update }

const DEFAULT_LAYOUT = () => ([{ id: uid(), type: "loops" }, { id: uid(), type: "approvals" }]);
function repoByName(n) { return state.repos.find(r => r.name === n); }
function statusOf(r) {
  const hb = r.heartbeat || {}; const st = hb.status || (r.running ? "running" : "stopped");
  const map = { iterating: ["run", "iterating"], starting: ["run", "starting"], sleeping: ["sleep", "sleeping"],
    idle: ["idle", "idle"], error: ["err", "error"], stopped: ["grey", "stopped"], running: ["run", "running"] };
  let [cls, label] = map[st] || ["grey", st];
  if (!r.running && st !== "error") { cls = "grey"; label = r.running === false ? "stopped" : label; }
  return { cls, label, phase: hb.phase || "" };
}

/* ---------- config writer (positional bridge call; null = unchanged) ---------- */
function setConfig(name, c) {
  return act("set_repo_config", name,
    c.provider ?? null, c.model ?? null, c.ship ?? null, null /*gate*/,
    c.pr_target_branch ?? null, null /*interval*/, null /*max_iterations*/,
    c.reasoning ?? null, null /*goal*/, null /*phases*/);
}

/* ---------- selector builders ---------- */
function selectField(label, value, options, onChange) {
  const f = h("div", "field");
  f.appendChild(h("label", null, label));
  const sel = h("select");
  options.forEach(([v, t]) => { const o = h("option"); o.value = v; o.textContent = t; if (v === value) o.selected = true; sel.appendChild(o); });
  sel.onchange = () => onChange(sel.value);
  f.appendChild(sel); f._sel = sel; return f;
}
function inputField(label, value, onCommit, listId, list) {
  const f = h("div", "field");
  f.appendChild(h("label", null, label));
  const inp = h("input"); inp.type = "text"; inp.value = value || "";
  if (listId) { inp.setAttribute("list", listId); if (list && !$("#" + listId)) { const dl = h("datalist"); dl.id = listId; list.forEach(v => { const o = h("option"); o.value = v; dl.appendChild(o); }); document.body.appendChild(dl); } }
  const commit = () => { if (inp.value !== (value || "")) onCommit(inp.value.trim()); };
  inp.onblur = commit; inp.onkeydown = e => { if (e.key === "Enter") inp.blur(); };
  f.appendChild(inp); f._inp = inp; return f;
}

/* ---------- panel: loop controls ---------- */
function loopRow(r) {
  const row = h("div", "loop");
  const top = h("div", "loop-top");
  const dot = h("span", "dot");
  const name = h("span", "loop-name", esc(r.name));
  const meta = h("span", "loop-meta");
  const spacer = h("span", "spacer");
  const btn = h("button", "btn sm");
  top.append(dot, name, meta, spacer, btn);

  const grid = h("div", "grid2");
  const fProv = selectField("Provider", r.provider, state.providers.map(p => [p, PROV[p] || p]), v => commit({ provider: v }));
  const fModel = inputField("Model", r.model, v => commit({ model: v }), "models", KNOWN_MODELS);
  const fShip = selectField("Ship mode", r.ship, SHIP, v => commit({ ship: v }));
  const fBranch = inputField("Target branch", r.pr_target_branch, v => commit({ pr_target_branch: v }));
  const fReason = selectField("Reasoning", r.reasoning || "high", REASON.map(x => [x, x]), v => commit({ reasoning: v }));
  fModel.classList.add("wide");
  grid.append(fProv, fModel, fShip, fBranch, fReason);
  row.append(top, grid);

  async function commit(c) {
    const x = await setConfig(r.name, c);
    toast(x && x.ok ? `${r.name}: ${Object.keys(c)[0]} updated` : `Update failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err");
    refresh();
  }
  async function toggle() {
    btn.disabled = true;
    const running = (repoByName(r.name) || r).running;
    const x = await act(running ? "stop" : "start", r.name);
    toast(x && x.ok ? `${r.name}: ${running ? "stopping" : "starting"}…` : `${(x && x.error) || "failed"}`, x && x.ok ? "ok" : "err");
    setTimeout(refresh, 600);
  }
  btn.onclick = toggle;

  function update() {
    const cur = repoByName(r.name) || r;
    const s = statusOf(cur);
    dot.className = "dot " + s.cls;
    meta.textContent = s.label + (s.phase ? " · " + s.phase : "") + (cur.heartbeat && cur.heartbeat.updated_at ? " · " + ago(cur.heartbeat.updated_at) : "");
    const running = cur.running;
    btn.classList.toggle("primary", !running);
    btn.classList.toggle("danger", running);
    btn.innerHTML = (running ? ic("stop", 14) : ic("play", 14)) + (running ? "Stop" : "Start");
    btn.disabled = false;
    // keep selectors in sync unless the operator is editing one
    const setSel = (f, v) => { if (f._sel && document.activeElement !== f._sel) f._sel.value = v; };
    const setInp = (f, v) => { if (f._inp && document.activeElement !== f._inp) f._inp.value = v || ""; };
    setSel(fProv, cur.provider); setInp(fModel, cur.model); setSel(fShip, cur.ship);
    setInp(fBranch, cur.pr_target_branch); setSel(fReason, cur.reasoning || "high");
  }
  return { el: row, update };
}

function loopsPanel(body) {
  const rows = new Map();
  function build() {
    body.innerHTML = "";
    rows.clear();
    if (!state.repos.length) { body.appendChild(h("p", "muted", "No repos registered. Add one in Settings.")); return; }
    state.repos.forEach(r => { const lr = loopRow(r); rows.set(r.name, lr); body.appendChild(lr.el); });
    const hint = h("p", "muted", "Everything else (gate, goal, contracts) is auto-configured by the PI agent on Start.");
    hint.style.cssText = "margin:10px 2px 0;font-size:11px"; body.appendChild(hint);
  }
  build();
  let names = state.repos.map(r => r.name).join();
  return { update() { const n = state.repos.map(r => r.name).join(); if (n !== names) { names = n; build(); } rows.forEach(lr => lr.update()); } };
}

/* ---------- panel: activity / log ---------- */
function activityPanel(body, spec) {
  const head = h("div", "act-head");
  const sel = h("select");
  const refreshLog = async () => { try { const x = await call("read_log", spec.repo); const txt = (x && x.text) || (typeof x === "string" ? x : (x && x.log) || ""); pre.textContent = txt || "(no log yet)"; const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 40; if (atBottom) pre.scrollTop = pre.scrollHeight; } catch { pre.textContent = "(log unavailable)"; } };
  const pre = h("div", "log", "(loading…)");
  function fillRepos() {
    sel.innerHTML = ""; state.repos.forEach(r => { const o = h("option"); o.value = r.name; o.textContent = r.name; if (r.name === spec.repo) o.selected = true; sel.appendChild(o); });
    if (!spec.repo && state.repos[0]) { spec.repo = state.repos[0].name; sel.value = spec.repo; saveLayout(); }
  }
  sel.onchange = () => { spec.repo = sel.value; saveLayout(); refreshLog(); };
  head.append(h("span", "muted", "Repo"), sel);
  body.append(head, pre);
  fillRepos(); refreshLog();
  return { update() { fillRepos(); refreshLog(); } };
}

/* ---------- panel: approvals / PRs ---------- */
function approvalsPanel(body) {
  function build() {
    body.innerHTML = "";
    const all = [];
    state.repos.forEach(r => (r.prs || []).forEach(p => all.push({ repo: r.name, pr: p })));
    if (!state.gh_ready) { body.appendChild(h("p", "muted", "GitHub not connected — sign in from Settings to see PRs.")); return; }
    if (!all.length) { body.appendChild(h("p", "muted", "No open pull requests. 🎉")); return; }
    all.forEach(({ repo, pr }) => {
      const row = h("div", "pr");
      const main = h("div", "pr-main");
      main.append(h("div", "pr-title", esc(pr.title || ("#" + pr.number))), h("div", "pr-sub", `${esc(repo)} · #${pr.number} ${pr.state ? "· " + esc(pr.state) : ""}`));
      const sp = h("span", "spacer");
      const merge = h("button", "btn sm primary", ic("merge", 13) + "Merge");
      const close = h("button", "btn sm danger", ic("x", 13) + "Close");
      merge.onclick = async () => { merge.disabled = true; const x = await act("merge", repo, pr.number); toast(x && x.ok ? `${repo} #${pr.number} merged` : `Merge failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); setTimeout(refresh, 700); };
      close.onclick = async () => { close.disabled = true; const x = await act("close", repo, pr.number); toast(x && x.ok ? `${repo} #${pr.number} closed` : `Close failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); setTimeout(refresh, 700); };
      row.append(main, sp, merge, close);
      body.appendChild(row);
    });
  }
  build();
  let sig = "";
  return { update() { const s = JSON.stringify(state.repos.map(r => [r.name, (r.prs || []).map(p => p.number)])); if (s !== sig) { sig = s; build(); } } };
}

/* ---------- panel shell ---------- */
function makePanel(spec) {
  const el = h("div", "panel" + (spec.span2 ? " span2" : ""));
  const head = h("div", "panel-head");
  const meta = PANEL_TYPES[spec.type] || { title: spec.type, icon: "" };
  const title = h("div", "panel-title", `<span class="dot-grip">${ic("grip", 16)}</span>${ic2(meta.icon)}<span>${esc(meta.title)}</span>`);
  const tools = h("div", "head-tools");
  const spanBtn = h("button", "head-btn", ic("expand", 15)); spanBtn.title = "Toggle wide";
  const closeBtn = h("button", "head-btn", ic("x", 15)); closeBtn.title = "Remove panel";
  spanBtn.onclick = () => { spec.span2 = !spec.span2; el.classList.toggle("span2", spec.span2); saveLayout(); };
  closeBtn.onclick = () => { layout = layout.filter(p => p.id !== spec.id); saveLayout(); renderWorkspace(); };
  tools.append(spanBtn, closeBtn);
  head.append(title, tools);
  const body = h("div", "panel-body");
  el.append(head, body);

  // drag to rearrange (handle = head)
  head.draggable = true;
  head.ondragstart = e => { e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", spec.id); dragId = spec.id; el.classList.add("dragging"); };
  head.ondragend = () => { el.classList.remove("dragging"); document.querySelectorAll(".panel.dragover").forEach(p => p.classList.remove("dragover")); };
  el.ondragover = e => { if (dragId && dragId !== spec.id) { e.preventDefault(); el.classList.add("dragover"); } };
  el.ondragleave = () => el.classList.remove("dragover");
  el.ondrop = e => { e.preventDefault(); el.classList.remove("dragover"); if (dragId && dragId !== spec.id) reorder(dragId, spec.id); };

  let api = { update() {} };
  if (spec.type === "loops") api = loopsPanel(body);
  else if (spec.type === "activity") api = activityPanel(body, spec);
  else if (spec.type === "approvals") api = approvalsPanel(body);
  return { el, update: api.update };
}
function ic2(inner) { return `<svg viewBox="0 0 24 24" width="16" height="16" class="ic" style="color:var(--clay)">${inner}</svg>`; }

let dragId = null;
function reorder(fromId, toId) {
  const from = layout.findIndex(p => p.id === fromId), to = layout.findIndex(p => p.id === toId);
  if (from < 0 || to < 0) return;
  const [m] = layout.splice(from, 1); layout.splice(to, 0, m);
  saveLayout(); renderWorkspace();
}

/* ---------- workspace render ---------- */
function renderWorkspace() {
  const ws = $("#workspace"); ws.innerHTML = ""; panels.clear();
  if (!layout.length) {
    const e = h("div", "empty", `<h2>Your workspace is empty</h2><p>Add a panel to get started.</p>`);
    const b = h("button", "btn primary", ic("expand", 15) + "Add a panel"); b.onclick = openBento;
    e.appendChild(b); ws.appendChild(e); return;
  }
  layout.forEach(spec => { const p = makePanel(spec); panels.set(spec.id, p); ws.appendChild(p.el); });
}
function applyState() { panels.forEach(p => { try { p.update(); } catch (e) { /* one panel must not break the rest */ } }); }

/* ---------- data ---------- */
async function refresh() { try { const s = await call("get_state"); Object.assign(state, s); applyState(); $("#version").textContent = ""; } catch (e) { /* keep prior */ } }
function saveLayout() { const clean = layout.map(({ id, type, repo, span2 }) => ({ id, type, repo, span2 })); act("set_layout", clean); try { localStorage.setItem("solomon.layout", JSON.stringify(clean)); } catch {} }

/* ---------- bento (add panel) ---------- */
function openBento() {
  const b = $("#bento"); b.innerHTML = "";
  Object.entries(PANEL_TYPES).forEach(([type, m]) => {
    const item = h("div", "bento-item", `${ic2(m.icon)}<div><b>${esc(m.title)}</b><small>${esc(m.desc)}</small></div>`);
    item.onclick = () => { layout.push({ id: uid(), type, repo: type === "activity" ? (state.repos[0] && state.repos[0].name) : undefined }); saveLayout(); renderWorkspace(); applyState(); closeBento(); };
    b.appendChild(item);
  });
  const r = $("#addPanel").getBoundingClientRect();
  // right-anchor so the popover never spills past the viewport edge
  b.style.top = (r.bottom + 8) + "px";
  b.style.left = "auto";
  b.style.right = Math.max(12, window.innerWidth - r.right) + "px";
  b.classList.remove("hidden");
  setTimeout(() => document.addEventListener("click", outsideBento), 0);
}
function closeBento() { $("#bento").classList.add("hidden"); document.removeEventListener("click", outsideBento); }
function outsideBento(e) { if (!$("#bento").contains(e.target) && e.target !== $("#addPanel")) closeBento(); }

/* ---------- settings drawer ---------- */
function openSettings() {
  const d = $("#settings"); d.innerHTML = "";
  d.append(h("h3", null, "Settings"), h("p", "sub", "Give the agent a key, a repo, and a branch — it configures the rest."));

  // API keys
  const keys = h("div", "sect", `<h4>API keys</h4>`);
  state.providers.forEach(p => {
    const row = h("div", "row");
    const inp = h("input"); inp.type = "password"; inp.placeholder = (state.keys && state.keys[p]) ? "•••••• (set)" : `${PROV[p] || p} key`;
    const save = h("button", "btn sm", "Save");
    save.onclick = async () => { if (!inp.value) return; const x = await act("set_key", p, inp.value); toast(x && x.ok ? `${PROV[p] || p} key saved` : `Failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); inp.value = ""; refresh(); };
    row.append(h("span", "lbl", PROV[p] || p), h("span", "spacer"));
    const wrap = h("div", "sect"); wrap.style.margin = "0 0 12px";
    const line = h("div", "row"); line.style.marginTop = "6px"; line.append(inp, save);
    keys.append(h("div", "row", `<span class="lbl">${esc(PROV[p] || p)}</span>`), line);
  });
  d.appendChild(keys);

  // GitHub
  const gh = h("div", "sect", `<h4>GitHub</h4>`);
  const ghRow = h("div", "row");
  const ghState = h("span", "lbl", state.gh_ready ? "Connected" : "Not connected");
  const ghBtn = h("button", "btn sm", ic("github", 14) + (state.gh_ready ? "Re-auth" : "Sign in"));
  ghBtn.onclick = async () => { const x = await act("github_login_start"); toast(x && x.ok ? "Follow the GitHub device prompt…" : `${(x && x.error) || "failed"}`, x && x.ok ? "ok" : "err"); };
  ghRow.append(ghState, h("span", "spacer"), ghBtn); gh.appendChild(ghRow); d.appendChild(gh);

  // Add project
  const add = h("div", "sect", `<h4>Add a repo</h4>`);
  const path = h("input"); path.type = "text"; path.placeholder = "C:\\path\\to\\repo  or  owner/repo";
  const addBtn = h("button", "btn sm primary", "Add");
  addBtn.onclick = async () => { if (!path.value.trim()) return; addBtn.disabled = true; const x = await act("add_project", path.value.trim()); toast(x && x.ok ? `Added ${x.name || ""} — provisioning…` : `Failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); path.value = ""; addBtn.disabled = false; refresh(); };
  const addLine = h("div", "row"); addLine.append(path, addBtn); add.append(addLine); d.appendChild(add);

  // Global
  const glob = h("div", "sect", `<h4>Global</h4>`);
  const apRow = h("div", "row");
  const sw = h("button", "switch" + (state.auto_push ? " on" : ""), '<span class="knob"></span>');
  sw.onclick = async () => { const v = !state.auto_push; const x = await act("set_auto_push", v); if (x && x.ok) { state.auto_push = v; sw.classList.toggle("on", v); } };
  apRow.append(h("span", "lbl", "Auto-push<small>push/PR finished work automatically</small>"), h("span", "spacer"), sw);
  glob.appendChild(apRow); d.appendChild(glob);

  $("#settingsScrim").classList.remove("hidden");
  d.classList.remove("hidden");
}
function closeSettings() { $("#settings").classList.add("hidden"); $("#settingsScrim").classList.add("hidden"); }

/* ---------- boot ---------- */
async function boot() {
  $("#addPanel").onclick = (e) => { e.stopPropagation(); $("#bento").classList.contains("hidden") ? openBento() : closeBento(); };
  $("#openSettings").onclick = openSettings;
  $("#settingsScrim").onclick = closeSettings;

  try { const s = await call("get_state"); Object.assign(state, s); } catch (e) { toast("Backend not ready: " + e.message, "err"); }
  let saved = null;
  try { saved = await call("get_layout"); } catch {}
  if (!Array.isArray(saved) || !saved.length) { try { saved = JSON.parse(localStorage.getItem("solomon.layout") || "null"); } catch {} }
  layout = (Array.isArray(saved) && saved.length ? saved : DEFAULT_LAYOUT()).map(p => ({ id: p.id || uid(), type: p.type, repo: p.repo, span2: !!p.span2 }));
  try { const sha = await call("current_sha"); $("#version").textContent = (sha && (sha.sha || sha)) ? String(sha.sha || sha).slice(0, 7) : ""; } catch {}

  renderWorkspace(); applyState();
  setInterval(refresh, 4000);
}

/* pywebview readiness: api is injected after load; also handle the already-ready + mock cases.
   The actual start() invocation is at the very bottom — AFTER `mock` is initialized, so a
   mock get_state() during boot() can't hit the const's temporal dead zone. */
let booted = false;
function start() { if (booted) return; booted = true; boot(); }

/* ---------- mock data (browser preview: index.html?mock=1) ---------- */
const mock = (() => {
  const repos = [
    { name: "maki", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "main", reasoning: "xhigh", running: true, heartbeat: { status: "iterating", phase: "implement", updated_at: new Date(Date.now() - 40000).toISOString() }, prs: [{ number: 142, title: "harden KCC convert pipeline", state: "open" }] },
    { name: "sover", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "main", reasoning: "xhigh", running: false, heartbeat: { status: "stopped" }, prs: [] },
    { name: "asmodeus", provider: "ollama-cloud", model: "glm-5.2", ship: "auto-merge", pr_target_branch: "master", reasoning: "high", running: true, heartbeat: { status: "sleeping", phase: "reflect", updated_at: new Date(Date.now() - 9000).toISOString() }, prs: [{ number: 88, title: "advance funnel candidate v54", state: "open" }] },
    { name: "dotz", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "master", reasoning: "xhigh", running: false, heartbeat: { status: "idle" }, prs: [] },
  ];
  let lay = null;
  return {
    get_state: () => ({ repos, providers: ["ollama-cloud", "openrouter"], gh_ready: true, keys: { "ollama-cloud": true }, github: { user: "cayleb" }, auto_push: true }),
    get_layout: () => lay, set_layout: (l) => { lay = l; return { ok: true }; },
    current_sha: () => ({ sha: "efd7ba2" }),
    read_log: (n) => ({ text: `2026-06-22T06:44Z iteration 3: branch rsi/iter — Pi working (${n})\n2026-06-22T06:45Z gate: pytest…\n2026-06-22T06:46Z Pi made one improvement; opening PR` }),
    set_repo_config: () => ({ ok: true }), start: () => ({ ok: true }), stop: () => ({ ok: true }),
    merge: () => ({ ok: true }), close: () => ({ ok: true }), set_key: () => ({ ok: true }),
    github_login_start: () => ({ ok: true }), add_project: () => ({ ok: true, name: "newrepo" }), set_auto_push: () => ({ ok: true }),
  };
})();

/* boot now that `mock` exists */
if (MOCK || realApi()) start();
else { window.addEventListener("pywebviewready", start); setTimeout(start, 4000); }
