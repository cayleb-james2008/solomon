/* Solomon v2 — cyberbrutalist fleet control over the pywebview Api bridge.
   Leads with FLEET TRUTH (ops probes + 24h business outcomes), the CEO rhythm
   (morning plan / evening report), and the incident feed; loop config, activity,
   and approvals remain as panels. Customizable workspace: open panels from the
   bento menu, drag to rearrange. Append ?mock=1 for sample data in a browser. */

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
  fleet: { title: "Fleet", icon: '<rect x="3" y="3" width="8" height="8" rx="1"/><rect x="13" y="3" width="8" height="8" rx="1"/><rect x="3" y="13" width="8" height="8" rx="1"/><rect x="13" y="13" width="8" height="8" rx="1"/>', desc: "Ground-truth probes + 24h outcomes per project" },
  ceo: { title: "CEO Rhythm", icon: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3.5 2"/>', desc: "Morning plan · evening report · deliveries" },
  incidents: { title: "Incidents", icon: '<path d="M12 3l10 18H2z"/><path d="M12 10v5"/><circle cx="12" cy="18" r="0.5"/>', desc: "Red / recovered transitions (24h)" },
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
const state = { repos: [], providers: ["ollama-cloud", "openrouter"], gh_ready: false, keys: {}, github: {}, auto_push: true, ops: null, fleet: null };
let layout = [];
const panels = new Map();   // id -> { el, update }

const DEFAULT_LAYOUT = () => ([
  { id: uid(), type: "fleet", span2: true },
  { id: uid(), type: "ceo" },
  { id: uid(), type: "incidents" },
  { id: uid(), type: "approvals" },
]);
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
    c.reasoning ?? null, null /*goal*/, null /*phases*/,
    c.api_key ?? null);
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
  // baseline is re-snapshotted at focus, not captured at build: a background refresh (setInp) can
  // rewrite inp.value while unfocused, so comparing against the build-time `value` would re-commit
  // that refreshed value on a no-op focus→blur. Snapshotting at focus makes blur a true no-op.
  let baseline = value || "";
  inp.onfocus = () => { baseline = inp.value; };
  const commit = () => { if (inp.value !== baseline) onCommit(inp.value.trim()); };
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
  // Per-repo API key (overrides the global .env key for this repo's iterations). Password input;
  // placeholder reflects whether a per-repo key is set (the value is never sent back by the backend).
  const fKey = h("div", "field wide");
  fKey.appendChild(h("label", null, "API key (per-repo)"));
  const keyInp = h("input");
  keyInp.type = "password";
  keyInp.setAttribute("autocomplete", "off");
  keyInp.setAttribute("aria-label", `${r.name} per-repo API key`);
  let keyBaseline = "";
  keyInp.onfocus = () => { keyBaseline = keyInp.value; };
  const commitKey = () => { if (keyInp.value !== keyBaseline) commit({ api_key: keyInp.value.trim() }); };
  keyInp.onblur = commitKey;
  keyInp.onkeydown = e => { if (e.key === "Enter") keyInp.blur(); };
  fKey.appendChild(keyInp);
  fModel.classList.add("wide");
  grid.append(fProv, fModel, fShip, fBranch, fReason, fKey);
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
    btn.disabled = false; // re-enable now (like addBtn) — a failed get_state refresh must not freeze the control
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
    // per-repo key: never display a value (backend doesn't return it); reflect set-state in the placeholder.
    if (document.activeElement !== keyInp) {
      keyInp.value = "";
      keyInp.placeholder = cur.api_key_set ? "•••••• (per-repo key set — type to replace)" : "uses global key (type to set per-repo)";
    }
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
// Richer activity surface: live metrics (iterations / shipped / reverted / success rate) + a tests
// sparkline + an iteration-timeline of status nodes ABOVE the tailing log. All data flows from the
// EXISTING bridge methods (metrics, read_history, read_log) — no new backend.
const STATUS_COLOR = { shipped: "--ok", reverted: "--err", noop: "--ink-3", blocked: "--warn", error: "--err", stopped: "--ink-3" };
const STATUS_GLYPH = { shipped: "✓", reverted: "↺", noop: "·", blocked: "!", error: "⚠", stopped: "○" };
function sparkline(series) {
  if (!series || !series.length) return '<svg class="spark" viewBox="0 0 120 28" aria-hidden="true"><text x="60" y="18" text-anchor="middle" class="spark-empty">no tests yet</text></svg>';
  const w = 120, h = 28, pad = 3;
  const vals = series.map(s => Number(s.passed || 0) + Number(s.failed || 0));
  const max = Math.max(1, ...vals);
  const pts = series.map((s, i) => {
    const x = pad + (i / Math.max(1, series.length - 1)) * (w - pad * 2);
    const y = h - pad - (Number(s.passed || 0) / max) * (h - pad * 2);
    return [x, y, Number(s.failed || 0)];
  });
  // Single-point degenerate case: a lone `M` renders no visible line. Draw a dot (passed) + bar (failed)
  // so one data point isn't an invisible empty graph.
  if (pts.length === 1) {
    const [x, y, f] = pts[0];
    const bh = (f / max) * (h - pad * 2);
    return `<svg class="spark" viewBox="0 0 ${w} ${h}" aria-hidden="true"><rect x="${(x - 1.2).toFixed(1)}" y="${(h - pad - bh).toFixed(1)}" width="2.4" height="${bh.toFixed(1)}" class="spark-bar" rx="0.6"/><circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="2.4" class="spark-line" fill="var(--clay)" stroke="none"/></svg>`;
  }
  const line = pts.map((p, i) => (i === 0 ? "M" : "L") + p[0].toFixed(1) + " " + p[1].toFixed(1)).join(" ");
  const area = line + ` L${pts[pts.length - 1][0].toFixed(1)} ${h - pad} L${pts[0][0].toFixed(1)} ${h - pad} Z`;
  const bars = pts.map(p => `<rect x="${(p[0] - 1.2).toFixed(1)}" y="${(h - pad - (p[2] / max) * (h - pad * 2)).toFixed(1)}" width="2.4" height="${((p[2] / max) * (h - pad * 2)).toFixed(1)}" class="spark-bar" rx="0.6"/>`).join("");
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" aria-hidden="true"><path d="${area}" class="spark-area"/><path d="${line}" class="spark-line"/><g>${bars}</g></svg>`;
}
function timelineNodes(hist) {
  if (!hist || !hist.length) return '<div class="tl-empty">no iterations yet</div>';
  const last = hist.slice(-24);
  return '<div class="tl">' + last.map(rec => {
    const st = rec.status || "noop";
    const col = STATUS_COLOR[st] || "--ink-3";
    const g = STATUS_GLYPH[st] || "·";
    const i = rec.iteration ?? "?";
    const ts = rec.ts ? ago(rec.ts) : "";
    const title = `iter ${i} · ${st}${ts ? " · " + ts : ""}`;
    return `<span class="tl-node ${st}" style="--node:var(${col})" title="${esc(title)}">${esc(g)}</span>`;
  }).join("") + '</div>';
}
function metricsRow(m) {
  if (!m || m.iterations == null) return '<div class="metrics-row muted">no metrics yet</div>';
  const sr = m.success_rate == null ? "—" : (Math.round(Number(m.success_rate) * 100) + "%");
  return '<div class="metrics-row">'
    + `<div class="mstat"><span class="mstat-val">${esc(m.iterations)}</span><span class="mstat-lbl">iters</span></div>`
    + `<div class="mstat ok"><span class="mstat-val">${esc(m.shipped || 0)}</span><span class="mstat-lbl">shipped</span></div>`
    + `<div class="mstat err"><span class="mstat-val">${esc(m.reverted || 0)}</span><span class="mstat-lbl">reverted</span></div>`
    + `<div class="mstat"><span class="mstat-val sr">${esc(sr)}</span><span class="mstat-lbl">success</span></div>`
    + '</div>';
}

function activityPanel(body, spec) {
  const head = h("div", "act-head");
  const sel = h("select");
  let firstLoad = true;
  // metrics + history widgets (rebuilt only when changed; cheap DOM swap)
  const overview = h("div", "act-overview", '<div class="metrics-row muted">no metrics yet</div>');
  const tlWrap = h("div", "act-tl", '<div class="tl-empty">no iterations yet</div>');
  const logWrap = h("div", "act-logwrap");
  const pre = h("div", "log", "(loading…)");
  logWrap.append(pre);
  body.append(head, overview, tlWrap, logWrap);

  // A live log must OPEN on the latest line. Capture the bottom-pinned state BEFORE replacing text
  // (setting textContent resets scrollTop). Force a jump to the bottom on the first load and on a
  // repo switch (force=true); otherwise only follow the tail when the user was already at the bottom,
  // so a periodic refresh never yanks them out of scrollback they're reading.
  const refreshLog = async (force = false) => { try { const x = await call("read_log", spec.repo); const txt = (x && x.text) || (typeof x === "string" ? x : (x && x.log) || ""); const wasAtBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 40; pre.textContent = txt || "(no log yet)"; if (force || firstLoad || wasAtBottom) pre.scrollTop = pre.scrollHeight; firstLoad = false; } catch { pre.textContent = "(log unavailable)"; } };
  // metrics + history: debounce-ish — rebuild the widgets only when the serialized payload changes,
  // so a 4s tick doesn't thrash the DOM (and flash) when nothing moved.
  let metricsSig = "", histSig = "";
  const refreshMetrics = async () => { try { const m = await call("metrics", spec.repo); const sig = JSON.stringify(m); if (sig !== metricsSig) { metricsSig = sig; overview.innerHTML = metricsRow(m) + sparkline(m && m.tests_series); } } catch {} };
  const refreshHistory = async () => { try { const hist = await call("read_history", spec.repo, 40); const sig = JSON.stringify(hist); if (sig !== histSig) { histSig = sig; tlWrap.innerHTML = timelineNodes(hist); } } catch {} };

  function fillRepos() {
    sel.innerHTML = ""; state.repos.forEach(r => { const o = h("option"); o.value = r.name; o.textContent = r.name; if (r.name === spec.repo) o.selected = true; sel.appendChild(o); });
    // reset spec.repo to a LIVE repo when it is unset OR names a repo that has been removed/renamed,
    // so it stays in sync with what the <select> actually displays (a stale name -> permanently blank log).
    if (state.repos.length && !state.repos.some(r => r.name === spec.repo)) { spec.repo = state.repos[0].name; sel.value = spec.repo; saveLayout(); }
  }
  sel.onchange = () => { spec.repo = sel.value; saveLayout(); firstLoad = true; metricsSig = ""; histSig = ""; refreshLog(true); refreshMetrics(); refreshHistory(); };
  head.append(h("span", "muted", "Repo"), sel);
  fillRepos(); refreshLog(); refreshMetrics(); refreshHistory();
  // Rebuild the <select> only when the repo set changes AND the dropdown isn't focused — an
  // unconditional 4s rebuild clobbers an open/keyboard-navigated dropdown. (name-set cache mirrors
  // the loops panel's update(); the activeElement focus-guard mirrors loopRow's setSel.) refreshLog
  // still runs every tick.
  let names = state.repos.map(r => r.name).join();
  return { update() {
    const n = state.repos.map(r => r.name).join();
    if (n !== names && document.activeElement !== sel) { names = n; fillRepos(); }
    refreshLog();
    refreshMetrics();
    refreshHistory();
  } };
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
      // Re-enable in a finally so a FAILED merge/close (PR stays open, unchanged signature -> no
      // rebuild) can be retried from the UI instead of being stuck disabled until an unrelated
      // PR-set change.
      merge.onclick = async () => { merge.disabled = true; try { const x = await act("merge", repo, pr.number); toast(x && x.ok ? `${repo} #${pr.number} merged` : `Merge failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); } finally { merge.disabled = false; } setTimeout(refresh, 700); };
      close.onclick = async () => { close.disabled = true; try { const x = await act("close", repo, pr.number); toast(x && x.ok ? `${repo} #${pr.number} closed` : `Close failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); } finally { close.disabled = false; } setTimeout(refresh, 700); };
      row.append(main, sp, merge, close);
      body.appendChild(row);
    });
  }
  build();
  let sig = "";
  // include gh_ready: a connect (gh_ready false->true) with an unchanged PR set must still rebuild,
  // else the panel stays stuck on "GitHub not connected" until a PR appears.
  return { update() { const s = JSON.stringify([state.gh_ready, state.repos.map(r => [r.name, (r.prs || []).map(p => p.number)])]); if (s !== sig) { sig = s; build(); } } };
}

/* ---------- panel: FLEET (v2 — probe truth + 24h outcomes) ---------- */
function fmtDelta(d) {
  if (d == null) return "?";
  const s = (d >= 0 ? "+" : "") + Number(d).toFixed(2);
  return s;
}
function fleetCard(name, p, oc, proof, job) {
  const cls = p.status || "grey";
  const card = h("div", "fcard " + cls);

  const top = h("div", "fcard-top");
  top.append(
    h("span", "fcard-name", esc(name)),
    h("span", "fcard-prio", "P" + esc(p.priority ?? oc.priority ?? "?")),
    h("span", "fcard-status " + cls, esc(cls)),
  );
  card.appendChild(top);

  // 24h outcomes — only what this project actually measures
  const stats = h("div", "fcard-outcomes");
  const stat = (val, lbl, tone) => {
    const s = h("div", "fstat");
    s.innerHTML = `<b class="${tone || ""}">${esc(val)}</b><span>${esc(lbl)}</span>`;
    return s;
  };
  const it = oc.iterations_24h, sh = oc.shipped_24h;
  stats.appendChild(stat(`${it ?? "?"}/${sh ?? "?"}`, "iters/ship", it === 0 ? "err" : ""));
  if (oc.posts_24h !== undefined) {
    stats.appendChild(stat(oc.posts_24h ?? "?", "posts 24h", oc.posts_24h === 0 ? "err" : "ok"));
    if ((oc.posts_missing_url_24h || 0) > 0) stats.appendChild(stat(oc.posts_missing_url_24h, "no-url!", "warn"));
  }
  if (oc.equity_usd !== undefined) {
    stats.appendChild(stat("$" + (oc.equity_usd ?? "?"), "equity"));
    const d = oc.equity_delta_24h;
    stats.appendChild(stat(fmtDelta(d), "Δ 24h", d > 0 ? "ok" : d < 0 ? "err" : "warn"));
    stats.appendChild(stat(oc.live_trades_24h ?? "?", "live trades", oc.live_trades_24h === 0 ? "err" : ""));
    stats.appendChild(stat(oc.fills_24h ?? "?", "fills"));
  }
  card.appendChild(stats);

  // probe chips (id → status color)
  const probes = p.probes || {};
  if (Object.keys(probes).length) {
    const chips = h("div", "fcard-probes");
    Object.entries(probes).forEach(([id, st]) => {
      chips.appendChild(h("span", "probe-chip " + esc(st), esc(id)));
    });
    card.appendChild(chips);
  }

  // reasons — the honest "why not green"
  if ((p.reasons || []).length) {
    card.appendChild(h("div", "fcard-reasons" + (cls === "red" ? " red" : ""), esc(p.reasons.join("\n"))));
  }

  if (job) {
    const j = h("div", "fleet-job");
    j.innerHTML = `<b>${esc(job.job || "job")}</b><span>${esc(job.state || "queued")} | P${esc(job.priority ?? "?")}</span><em>${esc(job.next_action || job.reason || "")}</em>`;
    card.appendChild(j);
  }
  if (proof) {
    const outcome = proof.outcome || "proof";
    const pr = h("div", "fcard-proof " + esc(outcome));
    pr.innerHTML = `<b>${esc(outcome)}</b><span>${esc(ago(proof.ts) || "fresh")}</span><em title="${esc(proof.summary || "")}">${esc(proof.summary || "")}</em>`;
    card.appendChild(pr);
  }

  // lane row: live status + start/stop (reuses the loops panel actions)
  const lane = h("div", "fcard-lane");
  const r = repoByName(name);
  const dot = h("span", "dot");
  const meta = h("span", "lane-meta");
  lane.append(dot, meta);
  if (p.restart_forbidden) lane.appendChild(h("span", "restart-forbidden", "restart forbidden"));
  lane.appendChild(h("span", "spacer"));
  if (r) {
    const btn = h("button", "btn sm");
    btn.onclick = async () => {
      btn.disabled = true;
      const running = (repoByName(name) || r).running;
      const x = await act(running ? "stop" : "start", name);
      toast(x && x.ok ? `${name}: ${running ? "stopping" : "starting"}…` : `${(x && x.error) || "failed"}`, x && x.ok ? "ok" : "err");
      btn.disabled = false;
      setTimeout(refresh, 600);
    };
    lane.appendChild(btn);
    card._laneBtn = btn;
  }
  card._dot = dot; card._meta = meta; card._name = name;
  card.appendChild(lane);
  return card;
}
function fleetPanel(body) {
  let sig = "";
  let cards = [];
  function agentBar(fleet) {
    const active = fleet.active || null;
    const cfg = fleet.config || {};
    const cooldown = fleet.cooldown || null;
    const daily = fleet.daily || {};
    const bar = h("div", "fleet-agent");
    const activeTxt = active ? `${active.repo} | ${active.job}` : "idle";
    const coolTxt = cooldown && cooldown.until ? cooldown.until : "ready";
    const q = (fleet.queue || []).length;
    bar.innerHTML = `<div class="agent-kv"><span>agent</span><b>${esc(activeTxt)}</b></div>`
      + `<div class="agent-kv"><span>provider</span><b>${esc(cfg.provider || "openrouter")}</b><em>${esc(cfg.model || "")}</em></div>`
      + `<div class="agent-kv"><span>budget</span><b>${esc(daily.calls || 0)}/${esc(cfg.daily_call_budget || "?")}</b></div>`
      + `<div class="agent-kv ${cooldown ? "warn" : "ok"}"><span>quota</span><b>${esc(coolTxt)}</b></div>`
      + `<div class="agent-kv"><span>queue</span><b>${esc(q)}</b></div>`;
    const actions = h("div", "fleet-agent-actions");
    const once = h("button", "btn sm primary", "Run once");
    once.onclick = async () => {
      once.disabled = true;
      const x = await act("fleet_once");
      toast(x && x.ok ? "Fleet cycle recorded" : `Fleet failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err");
      once.disabled = false;
      refresh();
    };
    const drain = h("button", "btn sm", "Drain 1");
    drain.onclick = async () => {
      drain.disabled = true;
      const x = await act("fleet_drain", 1);
      toast(x && x.ok ? "Fleet drain recorded" : `Drain failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err");
      drain.disabled = false;
      refresh();
    };
    actions.append(once, drain);
    bar.appendChild(actions);
    return bar;
  }
  function build() {
    body.innerHTML = "";
    cards = [];
    const o = state.ops || {};
    const fleet = o.fleet || state.fleet || {};
    const opsProjects = (o.ops && o.ops.projects) || {};
    const outProjects = (o.outcomes && o.outcomes.projects) || {};
    const proofs = fleet.proofs || {};
    const jobs = {};
    (fleet.queue || []).forEach(j => { if (j && j.repo) jobs[j.repo] = j; });
    if (fleet.active && fleet.active.repo) jobs[fleet.active.repo] = fleet.active;
    const names = [...new Set([...Object.keys(opsProjects), ...Object.keys(outProjects), ...Object.keys(proofs), ...Object.keys(jobs)])];
    if (!names.length) {
      body.appendChild(h("p", "muted", "No fleet data yet — the watchdog tick writes probe verdicts within ~2 minutes of launch."));
      return;
    }
    body.appendChild(agentBar(fleet));
    const note = h("div", "fleet-note");
    const bw = o.ops && o.ops.blind_window_s;
    note.innerHTML = `<span>probes checked ${esc(ago(o.ops && o.ops.checked_at) || "—")}</span>`
      + (bw > 600 ? `<span class="blind">· blind ${(bw / 3600).toFixed(1)}h before that (app was closed)</span>` : "");
    body.appendChild(note);
    const grid = h("div", "fleet-grid");
    names
      .map(n => [n, opsProjects[n] || {}, outProjects[n] || {}, proofs[n] || null, jobs[n] || null])
      .sort((a, b) => (a[1].priority ?? a[2].priority ?? 99) - (b[1].priority ?? b[2].priority ?? 99))
      .forEach(([n, p, oc, proof, job]) => { const c = fleetCard(n, p, oc, proof, job); cards.push(c); grid.appendChild(c); });
    body.appendChild(grid);
  }
  function updateLaneRows() {
    cards.forEach(c => {
      const r = repoByName(c._name);
      const s = r ? statusOf(r) : { cls: "grey", label: "not a lane", phase: "" };
      if (c._dot) c._dot.className = "dot " + s.cls;
      if (c._meta) c._meta.textContent = s.label + (s.phase ? " · " + s.phase : "") + (r && r.model ? " · " + r.model : "");
      if (c._laneBtn && r) {
        const running = r.running;
        c._laneBtn.classList.toggle("primary", !running);
        c._laneBtn.classList.toggle("danger", running);
        c._laneBtn.innerHTML = (running ? ic("stop", 13) : ic("play", 13)) + (running ? "Stop" : "Start");
      }
    });
  }
  build(); updateLaneRows();
  return { update() {
    const o = state.ops || {};
    const s = JSON.stringify([o.ops, o.outcomes, o.fleet, state.fleet]);
    if (s !== sig) { sig = s; build(); }
    updateLaneRows();
  } };
}

/* ---------- panel: CEO rhythm (v2 — plan / report / deliveries) ---------- */
function ceoPanel(body) {
  let sig = "";
  let tab = "report";
  function gateState(sec, today) {
    if (!sec) return ["pending", "pending"];
    // A give-up sentinel ({done, gave_up}) is stamped when the plan LLM call failed all attempts —
    // it must NOT render as a green 'done' (the day was never actually planned).
    if (sec.gave_up && sec.done === today) return ["failed", "gave up"];
    if (sec.done === today) return ["done", "done"];
    if ((sec.attempts || 0) > 0) return ["failed", `${sec.attempts} failed`];
    return ["pending", "pending"];
  }
  function build() {
    body.innerHTML = "";
    const o = state.ops || {};
    const c = o.ceo || {};
    const today = c.today || "";

    const day = h("div", "ceo-day");
    const [pCls, pTxt] = gateState(c.plan, today);
    const [sCls, sTxt] = gateState(c.summary, today);
    day.append(
      h("span", "ceo-gate", `plan ≥ ${esc(String(c.plan_hour ?? 7).padStart(2, "0"))}:00 <span class="st ${pCls}">${esc(pTxt)}</span>`),
      h("span", "ceo-gate", `report ≥ ${esc(String(c.summary_hour ?? 20).padStart(2, "0"))}:00 <span class="st ${sCls}">${esc(sTxt)}</span>`),
    );
    body.appendChild(day);

    const actions = h("div", "ceo-actions");
    const mkRun = (label, method) => {
      const b = h("button", "btn sm primary", esc(label));
      b.onclick = async () => {
        b.disabled = true;
        const x = await act(method);
        toast(x && x.ok ? `${label} started — result lands on the next refresh` : `${(x && x.error) || "failed"}`, x && x.ok ? "ok" : "err");
        setTimeout(() => { b.disabled = false; }, 4000);
      };
      return b;
    };
    actions.append(mkRun("Run plan now", "run_plan"), mkRun("Run report now", "run_report"));
    body.appendChild(actions);

    const tabs = h("div", "ceo-tabs");
    const md = h("div", "ceo-md");
    const renderMd = () => {
      const text = tab === "plan" ? (c.plan_md || "(no plan yet today)") : (c.report_md || "(no report yet today)");
      md.innerHTML = esc(text).split("\n").map(l => l.includes("⚠") ? `<span class="flag">${l}</span>` : l).join("\n");
    };
    ["report", "plan"].forEach(t => {
      const b = h("button", "ceo-tab" + (tab === t ? " active" : ""), t);
      b.onclick = () => { tab = t; tabs.querySelectorAll(".ceo-tab").forEach(x => x.classList.toggle("active", x.textContent === t)); renderMd(); };
      tabs.appendChild(b);
    });
    renderMd();
    body.append(tabs, md);

    const dl = h("div", "deliveries", "<h5>deliveries (ntfy · toast)</h5>");
    const tail = (o.notify_tail || []).slice(-6).reverse();
    if (!tail.length) dl.appendChild(h("div", "delivery", "none yet"));
    tail.forEach(n => {
      const okN = n.ntfy === "sent", okT = n.toast === "shown";
      dl.appendChild(h("div", "delivery",
        `<span>${esc(ago(n.ts))}</span><span>${esc(n.title || "")}</span>`
        + `<span class="${okN ? "ok" : "err"}">${okN ? "ntfy✓" : esc("ntfy:" + (n.ntfy || "?"))}</span>`
        + `<span class="${okT ? "ok" : "err"}">${okT ? "toast✓" : esc("toast:" + (n.toast || "?"))}</span>`));
    });
    body.appendChild(dl);
  }
  build();
  return { update() {
    const o = state.ops || {};
    const s = JSON.stringify([o.ceo, (o.notify_tail || []).length && o.notify_tail[o.notify_tail.length - 1]]);
    if (s !== sig) { sig = s; build(); }
  } };
}

/* ---------- panel: incidents (v2 — red/recovered transitions, 24h) ---------- */
function incidentsPanel(body) {
  let sig = "";
  function build() {
    body.innerHTML = "";
    const inc = ((state.ops || {}).incidents || []).slice().reverse();
    if (!inc.length) { body.appendChild(h("p", "muted", "No incidents in the last 24h. Quiet is only good when the Fleet panel is green.")); return; }
    inc.forEach(i => {
      const ev = i.event === "recovered" ? "recovered" : "red";
      const row = h("div", "inc " + ev);
      row.innerHTML = `<span class="inc-ev">${esc(ev)}</span><span class="inc-id">${esc(i.probe_id || "?")}</span>`
        + `<span class="inc-detail" title="${esc(i.detail || "")}">${esc(i.detail || "")}</span>`
        + `<span class="inc-ts">${esc(ago(i.ts))}</span>`;
      body.appendChild(row);
    });
  }
  build();
  return { update() {
    const s = JSON.stringify((state.ops || {}).incidents);
    if (s !== sig) { sig = s; build(); }
  } };
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
  if (spec.type === "fleet") api = fleetPanel(body);
  else if (spec.type === "ceo") api = ceoPanel(body);
  else if (spec.type === "incidents") api = incidentsPanel(body);
  else if (spec.type === "loops") api = loopsPanel(body);
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
async function refresh() {
  try { const s = await call("get_state"); Object.assign(state, s); } catch (e) { /* keep prior */ }
  // v2 fleet payload rides the same tick (server-side cached ~15s, so the 4s poll stays cheap).
  try { state.ops = await call("ops_state"); } catch (e) { /* keep prior */ }
  applyState();
}
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
    const inp = h("input"); inp.type = "password"; inp.placeholder = (state.keys && state.keys[p]) ? "•••••• (set)" : `${PROV[p] || p} key`;
    inp.setAttribute("aria-label", `${PROV[p] || p} API key`);
    const save = h("button", "btn sm", "Save");
    save.onclick = async () => { if (!inp.value) return; const x = await act("set_key", p, inp.value); toast(x && x.ok ? `${PROV[p] || p} key saved` : `Failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); inp.value = ""; refresh(); };
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
  const path = h("input"); path.type = "text"; path.placeholder = "C:\\path\\to\\repo  or  owner/repo"; path.setAttribute("aria-label", "Repo path or owner/repo");
  const addBtn = h("button", "btn sm primary", "Add");
  addBtn.onclick = async () => { if (!path.value.trim()) return; addBtn.disabled = true; const x = await act("add_project", path.value.trim()); toast(x && x.ok ? `Added ${x.name || ""} — provisioning…` : `Failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err"); path.value = ""; addBtn.disabled = false; refresh(); };
  const addLine = h("div", "row"); addLine.append(path, addBtn); add.append(addLine); d.appendChild(add);

  // Global
  const glob = h("div", "sect", `<h4>Global</h4>`);
  const apRow = h("div", "row");
  const sw = h("button", "switch" + (state.auto_push ? " on" : ""), '<span class="knob"></span>');
  sw.onclick = async () => { const v = !state.auto_push; const x = await act("set_auto_push", v); if (x && x.ok) { state.auto_push = v; sw.classList.toggle("on", v); } else { toast(`Auto-push change failed: ${(x && x.error) || "?"}`, "err"); } };
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
  try { state.ops = await call("ops_state"); } catch {}
  let saved = null;
  try { saved = await call("get_layout"); } catch {}
  if (!Array.isArray(saved) || !saved.length) { try { saved = JSON.parse(localStorage.getItem("solomon.layout") || "null"); } catch {} }
  layout = (Array.isArray(saved) && saved.length ? saved : DEFAULT_LAYOUT()).map(p => ({ id: p.id || uid(), type: p.type, repo: p.repo, span2: !!p.span2 }));
  // v2 migration (once): a layout saved before the fleet-first redesign gets the new planes
  // prepended so the redesign is what the operator actually sees. The one-shot flag means a
  // deliberately-removed fleet panel never comes back on its own.
  let migrated = false;
  try { migrated = localStorage.getItem("solomon.v2fleet") === "1"; } catch {}
  if (!migrated && !layout.some(p => p.type === "fleet" || p.type === "ceo")) {
    layout = [
      { id: uid(), type: "fleet", span2: true },
      { id: uid(), type: "ceo" },
      { id: uid(), type: "incidents" },
    ].concat(layout);
    try { localStorage.setItem("solomon.v2fleet", "1"); } catch {}
    saveLayout();
  }
  try { const sha = await call("current_sha"); $("#version").textContent = (sha && (sha.sha || sha)) ? String(sha.sha || sha).slice(0, 7) : ""; } catch {}
  // Render the dashboard NOW — first paint must NOT be gated on the network update check below.
  renderWorkspace(); applyState();
  setInterval(refresh, 4000);

  // Auto-update (background, non-blocking): a newer signed release (tauri-plugin-updater checks GitHub
  // Releases) turns the version label into a clickable "Update available" pill. Click -> apply_update()
  // downloads, installs (NSIS), relaunches. Detached so a slow/offline GitHub round-trip can't hold the
  // "Loading Solomon…" spinner up. ponytail: reuse the existing #version slot.
  (async () => {
    try {
      const u = await call("update_status");
      if (u && u.available) {
        const v = $("#version");
        v.textContent = "⬆ Update available";
        v.style.cursor = "pointer";
        v.title = "Click to update Solomon" + (u.version ? " to " + u.version : "");
        v.onclick = async () => { v.textContent = "updating…"; const r = await act("apply_update"); if (r && r.ok === false) { v.textContent = "update failed"; toast("Update failed: " + ((r && r.error) || "?"), "err"); } };
      }
    } catch {}
  })();
}

/* pywebview readiness: api is injected after load; also handle the already-ready + mock cases.
   The actual start() invocation is at the very bottom — AFTER `mock` is initialized, so a
   mock get_state() during boot() can't hit the const's temporal dead zone. */
let booted = false;
function start() { if (booted) return; booted = true; boot(); }

/* ---------- mock data (browser preview: index.html?mock=1) ---------- */
const mock = (() => {
  const repos = [
    { name: "maki", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "main", reasoning: "xhigh", running: true, api_key_set: false, heartbeat: { status: "iterating", phase: "implement", updated_at: new Date(Date.now() - 40000).toISOString() }, prs: [{ number: 142, title: "harden KCC convert pipeline", state: "open" }] },
    { name: "sover", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "main", reasoning: "xhigh", running: false, api_key_set: false, heartbeat: { status: "stopped" }, prs: [] },
    { name: "asmodeus", provider: "ollama-cloud", model: "glm-5.2", ship: "auto-merge", pr_target_branch: "master", reasoning: "high", running: true, api_key_set: true, heartbeat: { status: "sleeping", phase: "reflect", updated_at: new Date(Date.now() - 9000).toISOString() }, prs: [{ number: 88, title: "advance funnel candidate v54", state: "open" }] },
    { name: "dotz", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "auto-merge", pr_target_branch: "master", reasoning: "xhigh", running: false, api_key_set: false, heartbeat: { status: "idle" }, prs: [] },
  ];
  let lay = null;
  return {
    get_state: () => ({ repos, providers: ["ollama-cloud", "openrouter"], gh_ready: true, keys: { "ollama-cloud": true }, github: { user: "cayleb" }, auto_push: true, fleet: {
      config: { mode: "single_fleet", provider: "openrouter", model: "nvidia/nemotron-3-ultra-550b-a55b:free", daily_call_budget: 40 },
      active: { repo: "sover", job: "implement", priority: 10 },
      queue: [{ repo: "asmodeus", job: "proof_required", state: "proof_required", priority: 5, next_action: "surface blocker" }],
      cooldown: null,
      daily: { date: "2026-07-04", calls: 3 },
      proofs: { sover: { ts: new Date(Date.now() - 600000).toISOString(), outcome: "proof_required", summary: "posting blocker captured without retry storm" } },
    } }),
    get_layout: () => lay, set_layout: (l) => { lay = l; return { ok: true }; },
    current_sha: () => ({ sha: "efd7ba2" }),
    // v2 fleet payload (shape byte-identical to api::ops_state).
    ops_state: () => ({
      ops: {
        checked_at: new Date(Date.now() - 90000).toISOString().replace(/\.\d+Z$/, "Z"),
        blind_window_s: 8200,
        projects: {
          asmodeus: { priority: 1, status: "yellow", worst_probe: "auth_health", reasons: ["auth_health=yellow (188 yellow-pattern)"], restart_forbidden: false, probes: { fills_recency: "green", equity_fresh: "green", auth_health: "yellow", kill_breaker: "green", process: "green" } },
          sover: { priority: 2, status: "red", worst_probe: "publish_recency", reasons: ["publish_recency=red (age 26.1h)"], restart_forbidden: false, probes: { publish_recency: "red", cdp_alive: "green", autopost_gate: "green", process: "green" } },
          daedulus: { priority: 3, status: "yellow", worst_probe: "outcome_streak", reasons: ["outcome_streak=yellow (unobservable)"], restart_forbidden: false, probes: { outcome_streak: "yellow", heartbeat_fresh: "yellow" } },
          dotz: { priority: 4, status: "green", worst_probe: null, reasons: [], restart_forbidden: false, probes: { outcome_streak: "green", heartbeat_fresh: "green" } },
        },
      },
      outcomes: { projects: {
        asmodeus: { priority: 1, iterations_24h: 39, shipped_24h: 19, equity_usd: 168.97, equity_delta_24h: -3.71, live_trades_24h: 2, fills_24h: 4 },
        sover: { priority: 2, iterations_24h: 82, shipped_24h: 43, posts_24h: 0, posts_missing_url_24h: 1, last_post_at: "2026-06-30T19:43:53Z" },
        daedulus: { priority: 3, iterations_24h: 0, shipped_24h: 0 },
        dotz: { priority: 4, iterations_24h: 68, shipped_24h: 41 },
      } },
      fleet: {
        config: { mode: "single_fleet", provider: "openrouter", model: "nvidia/nemotron-3-ultra-550b-a55b:free", daily_call_budget: 40 },
        active: { repo: "sover", job: "implement", priority: 10 },
        queue: [{ repo: "asmodeus", job: "proof_required", state: "proof_required", priority: 5, next_action: "surface blocker" }],
        cooldown: null,
        daily: { date: "2026-07-04", calls: 3 },
        proofs: {
          sover: { ts: new Date(Date.now() - 600000).toISOString(), outcome: "proof_required", summary: "posting blocker captured without retry storm" },
          asmodeus: { ts: new Date(Date.now() - 3600000).toISOString(), outcome: "blocked", summary: "restart forbidden by live-money probes" },
        },
      },
      ceo: {
        today: "2026-07-02", plan_hour: 7, summary_hour: 20,
        plan: { done: "2026-07-02" }, summary: {},
        plan_md: "# Solomon morning plan — 2026-07-02\n\n## asmodeus\n- **goal** [refactor]: decouple the equity writers\n- **why**: the fitness signal is corrupted\n",
        report_md: "# Solomon evening report — 2026-07-01\n\nfleet: asmodeus YELLOW(auth_health) sover RED(publish_recency)\n\n## sover (priority 2)\n- ⚠ sover: ZERO posts in 24h\n",
      },
      incidents: [
        { ts: new Date(Date.now() - 7200000).toISOString(), event: "red", probe_id: "sover/publish_recency", detail: "age 26.1h > 24h" },
        { ts: new Date(Date.now() - 300000).toISOString(), event: "recovered", probe_id: "solomon/monitor_fresh", detail: "age 45s" },
      ],
      notify_tail: [
        { ts: new Date(Date.now() - 7200000).toISOString(), title: "Solomon: sover/publish_recency RED", priority: "urgent", ntfy: "sent", toast: "shown" },
        { ts: new Date(Date.now() - 300000).toISOString(), title: "Solomon: solomon/monitor_fresh recovered", priority: "default", ntfy: "sent", toast: "error: powershell exit 1" },
      ],
    }),
    run_plan: () => ({ ok: true, started: true }),
    run_report: () => ({ ok: true, started: true }),
    read_log: (n) => ({ text: `2026-06-22T06:44Z iteration 3: branch rsi/iter — Pi working (${n})\n2026-06-22T06:45Z gate: pytest…\n2026-06-22T06:46Z Pi made one improvement; opening PR` }),
    // mock metrics + history so ?mock=1 previews the rich activity panel (shape byte-identical to the
    // real backend: control.metrics / control.read_history).
    metrics: (n) => {
      const r = (n === "maki") ? { iterations: 142, shipped: 96, reverted: 31, success_rate: 0.736 }
        : (n === "asmodeus") ? { iterations: 88, shipped: 60, reverted: 21, success_rate: 0.706 }
        : { iterations: 7, shipped: 5, reverted: 1, success_rate: 0.833 };
      return { ...r, merged: r.shipped, noop: 3, blocked: 0, error: 1, stopped: 0,
        tests_series: Array.from({ length: 8 }, (_, i) => ({ ts: `t${i}`, passed: 4 + i, failed: i === 3 ? 2 : 0 })) };
    },
    read_history: (n, limit = 40) => {
      const seq = ["shipped", "shipped", "reverted", "shipped", "noop", "shipped", "blocked", "shipped", "error", "shipped"];
      const out = [];
      for (let i = 1; i <= Math.min(limit, 12); i++) {
        out.push({ iteration: i, status: seq[(i - 1) % seq.length], ts: new Date(Date.now() - (12 - i) * 3600 * 1000).toISOString() });
      }
      return out;
    },
    set_repo_config: () => ({ ok: true }), start: () => ({ ok: true }), stop: () => ({ ok: true }),
    merge: () => ({ ok: true }), close: () => ({ ok: true }), set_key: () => ({ ok: true }),
    github_login_start: () => ({ ok: true }), add_project: () => ({ ok: true, name: "newrepo" }), set_auto_push: () => ({ ok: true }),
  };
})();

/* boot now that `mock` exists */
if (MOCK || realApi()) start();
else { window.addEventListener("pywebviewready", start); setTimeout(start, 4000); }
