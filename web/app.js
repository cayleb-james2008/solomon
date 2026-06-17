/* Solomon — IDE-style control panel for cross-repo recursive self-improvement.
   Vanilla JS over the pywebview Api bridge. Append ?mock=1 to render with sample
   data in a plain browser (no backend) for visual verification. */
const $ = (s, r = document) => r.querySelector(s);
const el = (t, c, h) => { const e = document.createElement(t); if (c) e.className = c; if (h != null) e.innerHTML = h; return e; };
const esc = (s) => (s == null ? "" : String(s).replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c])));
const MOCK = /[?&]mock/.test(location.search);

const THEMES = [["dark", "#e08a63"], ["paper", "#d97757"], ["sakura", "#e0568a"], ["ink", "#161616"], ["cyber", "#c4a0ff"]];
const PROVIDER_LABEL = { "ollama-cloud": "Ollama Cloud", "openrouter": "OpenRouter" };
const SHIP_MODES = [["local", "Local"], ["push", "Push"], ["pr", "PR"], ["auto-merge", "Auto-merge"]];
const REASONING = ["", "off", "minimal", "low", "medium", "high", "xhigh"];
const NAV = [["home", "Home"], ["approvals", "Approvals"], ["history", "History"], ["console", "Console"], ["settings", "Settings"]];

const state = { theme: "dark", auto_push: true, auto_ai_fix: false, repos: [], gh_ready: false, keys: {}, github: {},
  providers: ["ollama-cloud", "openrouter"], view: "home", openRepo: null, wsTab: "activity",
  selPR: null, diffCache: {}, consoleRepo: null, histRepo: null };

/* ---------- icons (24x24 stroke) ---------- */
const P = {
  home: '<path d="M3 11l9-8 9 8"/><path d="M5 10v10h14V10"/>',
  approvals: '<rect x="6" y="3.5" width="12" height="17.5" rx="2"/><path d="M9.5 3.5h5v3h-5z"/><path d="M9 13l2 2 4-4"/>',
  history: '<circle cx="12" cy="12" r="8"/><path d="M12 8v4l3 2"/>',
  console: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 10l3 2-3 2"/><path d="M13 15h4"/>',
  settings: '<circle cx="12" cy="12" r="3"/><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1"/>',
  search: '<circle cx="11" cy="11" r="6"/><path d="M20 20l-4-4"/>',
  play: '<path d="M8 5l11 7-11 7z" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6.5" y="6.5" width="11" height="11" rx="1.5" fill="currentColor" stroke="none"/>',
  broom: '<path d="M4 19h7"/><path d="M14 4l6 6-5.5 3.5L9 8z"/><path d="M9 8l-4 7"/>',
  sparkle: '<path d="M12 4l1.7 4.3L18 10l-4.3 1.7L12 16l-1.7-4.3L6 10l4.3-1.7z"/>',
  check: '<path d="M5 12l4 4 10-10"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  bolt: '<path d="M13 3L5 13h6l-1 8 8-10h-6z"/>',
  trash: '<path d="M5 7h14M9 7V5h6v2M7 7l1 12h8l1-12"/>',
};
function icon(name, size = 18) {
  return `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${P[name] || ""}</svg>`;
}

/* ---------- api ---------- */
function realApi() { return (window.pywebview && window.pywebview.api) || null; }
async function call(m, ...a) {
  if (MOCK || !realApi()) { if (mock[m]) return mock[m](...a); throw new Error("mock missing: " + m); }
  const x = realApi();
  if (!x || !x[m]) throw new Error("API not ready");
  return x[m](...a);
}

function toast(msg, type = "") {
  const t = el("div", "toast " + type, esc(msg)); $("#toasts").appendChild(t);
  setTimeout(() => { t.style.opacity = "0"; t.style.transition = ".3s"; setTimeout(() => t.remove(), 320); }, 3400);
}
function ago(iso) {
  if (!iso) return ""; const t = Date.parse(iso); if (isNaN(t)) return "";
  const s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return s + "s ago"; if (s < 3600) return Math.round(s / 60) + "m ago";
  if (s < 86400) return Math.round(s / 3600) + "h ago"; return Math.round(s / 86400) + "d ago";
}

/* ---------- theme + topbar ---------- */
function applyTheme(t) {
  const valid = THEMES.some(([v]) => v === t) ? t : "dark";
  document.documentElement.dataset.theme = valid; state.theme = valid;
  $("#themeRow").querySelectorAll(".theme-chip").forEach(b => b.classList.toggle("active", b.dataset.t === valid));
}
function buildThemeRow() {
  const row = $("#themeRow"); row.innerHTML = "";
  THEMES.forEach(([v, c]) => {
    const b = el("button", "theme-chip"); b.dataset.t = v; b.style.setProperty("--sw", c); b.title = v;
    b.onclick = () => { applyTheme(v); call("set_theme", v).catch(() => {}); };
    row.appendChild(b);
  });
}
function syncTopbar() {
  $("#autoPush").classList.toggle("on", !!state.auto_push);
  $("#themeRow").querySelectorAll(".theme-chip").forEach(b => b.classList.toggle("active", b.dataset.t === state.theme));
}

/* ---------- rail ---------- */
function buildRail() {
  const rail = $("#rail"); rail.innerHTML = "";
  NAV.forEach(([v, label]) => {
    const item = el("button", "rail-item"); item.dataset.v = v;
    item.innerHTML = `${icon(v, 19)}<span>${label}</span>`;
    item.onclick = () => setView(v);
    rail.appendChild(item);
    if (v === "console") rail.appendChild(el("div", "rail-spacer"));
  });
}
function syncRail() {
  const open = state.repos.reduce((n, r) => n + ((r.prs || []).length), 0);
  $("#rail").querySelectorAll(".rail-item").forEach(it => {
    it.classList.toggle("active", it.dataset.v === state.view);
    if (it.dataset.v === "approvals") {
      let b = it.querySelector(".rail-badge");
      if (open > 0) { if (!b) { b = el("span", "rail-badge"); it.appendChild(b); } b.textContent = open; }
      else if (b) b.remove();
    }
  });
}

/* ---------- status ---------- */
function statusInfo(r) {
  const hb = r.heartbeat || {}; const st = hb.status || (r.running ? "running" : "stopped");
  const map = { iterating: ["run", "iterating"], starting: ["run", "starting"], idle: ["grey", "idle"],
    sleeping: ["slp", "sleeping"], error: ["err", "error"], stopped: ["grey", "stopped"], running: ["run", "running"] };
  let [cls, label] = map[st] || ["grey", st];
  if (!r.running && st !== "stopped" && st !== "error") { cls = "grey"; label = "offline"; }
  return { cls, label };
}

/* ---------- supervisor health ---------- */
function diagState(r) {
  const d = r.diagnosis || { category: "ok", healthy: true };
  if (r.escalation) return { cls: "err", label: String(r.escalation.category || "escalated").replace(/_/g, " "), flagged: true, esc: true, ev: r.escalation.evidence || d.evidence || "" };
  if (d.category && d.category !== "ok") return { cls: "warn", label: String(d.category).replace(/_/g, " "), flagged: true, esc: false, ev: d.evidence || "" };
  return { cls: "ok", label: "ok", flagged: false, esc: false, ev: d.evidence || "" };
}
async function doSupervise(name, allowPi) {
  const x = await call("supervise", name, !!allowPi);
  if (!x || !x.ok) { toast("Supervise: " + ((x && x.error) || "unavailable"), "err"); return; }
  const res = x.results || [];
  const r = res.find(z => z.name === name) || res[0];
  if (r) toast(`${r.name}: ${r.escalate ? "escalated — " + (r.message || "") : (r.message || r.category)}`, r.escalate ? "err" : "ok");
  else toast("Supervisor: nothing to do", "ok");
  refresh();
}

/* ================= VIEWS ================= */
function setView(v) { state.view = v; state.selPR = null; render(); }

function render() {
  syncTopbar(); syncRail();
  $("#shell").classList.toggle("no-dock", state.view !== "home");
  renderView();
  renderDock();
}

function renderView() {
  const v = $("#view");
  if (state.view === "home") return renderHome(v);
  if (state.view === "approvals") return renderApprovals(v);
  if (state.view === "history") return renderHistory(v);
  if (state.view === "console") return renderConsole(v);
  if (state.view === "settings") return renderSettings(v);
}

function needsOnboarding() {
  const noKeys = !Object.values(state.keys || {}).some(Boolean);
  return (!state.repos.length || noKeys || !state.gh_ready);
}

function renderHome(v) {
  if (needsOnboarding() && !state.repos.length) return renderOnboarding(v);
  const running = state.repos.filter(r => r.running).length;
  const prs = state.repos.reduce((n, r) => n + ((r.prs || []).length), 0);
  v.innerHTML = `<p class="eyebrow">Overview</p><h1 class="view-title">Repositories</h1>
    <p class="view-sub">${running} iterating &middot; ${prs} pull request${prs === 1 ? "" : "s"} open</p>
    <div class="cards" id="cards"></div>`;
  const wrap = $("#cards", v);
  state.repos.forEach(r => wrap.appendChild(repoCard(r)));
  const add = el("button", "add-card", `<span class="plus">+</span><span>Add repository</span>`);
  add.onclick = () => setView("settings");
  wrap.appendChild(add);
}

function repoCard(r) {
  const hb = r.heartbeat || {}; const si = statusInfo(r);
  const tests = hb.tests;
  const testStr = tests ? (tests.failed || tests.errors
    ? `<span class="red">${esc(tests.failed || 0)} failed</span>`
    : `<span class="green">${esc(tests.passed || 0)} passed</span>`) : "&mdash;";
  const ds = diagState(r);
  const card = el("div", "card clickable");
  card.innerHTML = `
    <div class="card-head">
      <span class="card-name"><span class="hdot ${ds.cls}" title="${esc(ds.ev)}"></span>${esc(r.name)}</span>
      <span class="pill ${si.cls}">${esc(si.label)}</span>
    </div>
    <p class="card-goal ${hb.goal ? "" : "muted"}">${hb.goal ? esc(hb.goal) : (r.running ? "starting…" : "idle")}</p>
    <div class="card-stats">
      <span>iter <b>${esc(hb.iteration ?? 0)}</b></span>
      <span>tests ${testStr}</span>
    </div>
    <div class="chips">
      <span class="chip">ship: ${esc(r.ship || "pr")}</span>
      <span class="chip key">PR &rarr; ${esc(r.pr_target_branch || "main")}</span>
      <span class="chip">reasoning: ${esc(r.reasoning || "default")}</span>
    </div>
    ${ds.flagged ? `<div class="sup-note ${ds.cls}">${icon("bolt", 13)}<span>${esc(ds.esc ? "needs you: " + ds.label : ds.label)}</span></div>` : ""}
    <div class="card-foot"></div>`;
  const foot = $(".card-foot", card);
  const canStart = r.is_git || r.running;
  const tog = el("button", "btn sm grow " + (r.running ? "danger" : "accent"),
    `${icon(r.running ? "stop" : "play", 14)}${r.running ? "Stop" : "Start"}`);
  tog.disabled = !canStart;
  tog.onclick = async (e) => {
    e.stopPropagation(); tog.disabled = true;
    const x = r.running ? await call("stop", r.name) : await call("start", r.name);
    toast(x.ok ? `${r.name}: ${r.running ? "stopping" : "started"}` : `Failed: ${x.error}`, x.ok ? "ok" : "err");
    refresh();
  };
  foot.appendChild(tog);
  if (ds.flagged) {
    const sup = el("button", "btn sm", `${icon("bolt", 14)}Supervise`);
    sup.title = ds.ev || "recover this agent";
    sup.onclick = async (e) => { e.stopPropagation(); sup.disabled = true; await doSupervise(r.name, false); };
    foot.appendChild(sup);
  }
  card.onclick = () => openWorkspace(r.name);
  return card;
}

function renderOnboarding(v) {
  const k = state.keys || {}; const hasKey = Object.values(k).some(Boolean);
  v.innerHTML = `<div class="watermark">&#8734;</div>
    <div class="onboard">
      <p class="eyebrow">Welcome</p>
      <h2>Set Solomon loose on any project</h2>
      <p class="lead">Three steps and an agent will recursively improve your repo &mdash; testing, committing, and opening pull requests for you to approve.</p>
      <div class="step ${hasKey ? "done" : ""}">
        <div class="step-num">1</div>
        <div class="step-main"><h4>Add your API key</h4><p>An Ollama Cloud or OpenRouter key powers the agent.</p>
          <div class="step-row"><input class="input" id="obKey" type="password" placeholder="paste API key" style="max-width:280px" />
            <select class="select" id="obProv" style="max-width:150px">${state.providers.map(p => `<option value="${p}">${esc(PROVIDER_LABEL[p] || p)}</option>`).join("")}</select>
            <button class="btn sm accent" id="obKeySave">Save</button></div></div>
      </div>
      <div class="step ${state.gh_ready ? "done" : ""}">
        <div class="step-num">2</div>
        <div class="step-main"><h4>Connect GitHub</h4><p>${state.gh_ready ? `Connected as <b>${esc((state.github || {}).login || "you")}</b>.` : `Run <code>gh auth login</code> in a terminal so Solomon can open PRs.`}</p></div>
      </div>
      <div class="step ${state.repos.length ? "done" : ""}">
        <div class="step-num">3</div>
        <div class="step-main"><h4>Add a repository</h4><p>Any GitHub repo (<code>owner/repo</code>) or local project.</p>
          <div class="step-row"><input class="input" id="obRepo" type="text" placeholder="owner/repo or GitHub URL" style="max-width:300px" />
            <button class="btn sm accent" id="obAdd">Add project</button></div></div>
      </div>
    </div>`;
  const ks = $("#obKeySave", v); if (ks) ks.onclick = async () => {
    const val = $("#obKey", v).value; if (!val) return toast("Enter a key first", "err");
    const x = await call("set_key", $("#obProv", v).value, val);
    toast(x.ok ? "Key saved" : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh();
  };
  const add = $("#obAdd", v); if (add) add.onclick = async () => {
    const spec = $("#obRepo", v).value.trim(); if (!spec) return;
    const x = await call("add_project", spec);
    toast(x.ok ? `Added ${x.name}${x.enriching ? " — enriching contract…" : ""}` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh();
  };
}

/* ---------- approvals dock (home) ---------- */
function allPRs() { return state.repos.flatMap(r => (r.prs || []).map(p => ({ repo: r.name, ...p }))); }
function renderDock() {
  const dock = $("#dock"); if (state.view !== "home") { dock.innerHTML = ""; return; }
  const prs = allPRs();
  dock.innerHTML = `<div class="dock-head"><h3>Approvals</h3><span class="count">${prs.length} open</span></div>
    <div class="dock-actions"></div><div class="dock-list"></div>`;
  if (!state.gh_ready) { $(".dock-list", dock).innerHTML = `<div class="empty">Connect GitHub to review PRs.</div>`; return; }
  if (prs.length) {
    const mab = el("button", "btn sm accent", `${icon("check", 14)}Merge all green`);
    mab.style.width = "100%"; mab.onclick = mergeAllGreen; $(".dock-actions", dock).appendChild(mab);
  }
  const list = $(".dock-list", dock);
  if (!prs.length) { list.innerHTML = `<div class="empty">No open rsi/* PRs.</div>`; return; }
  prs.forEach(p => list.appendChild(prRow(p)));
}
function prRow(p, selectable) {
  const row = el("div", "pr");
  row.innerHTML = `
    <div class="pr-top"><span class="check ${p.checks || "none"}"></span>${esc(p.repo)} #${esc(p.number)}</div>
    <div class="pr-title">${esc(p.title)}</div>
    <div class="pr-branch">${esc(p.headRefName || "")}</div>
    <div class="pr-acts">
      <button class="btn sm accent grow pr-m">${icon("check", 13)}Merge</button>
      <button class="btn sm danger grow pr-c">${icon("x", 13)}Close</button>
    </div>`;
  $(".pr-m", row).onclick = async (e) => { e.stopPropagation(); e.target.closest("button").disabled = true;
    const x = await call("merge", p.repo, p.number);
    toast(x.ok ? `${p.repo} #${p.number} merged` : `Merge failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); };
  $(".pr-c", row).onclick = async (e) => { e.stopPropagation(); e.target.closest("button").disabled = true;
    const x = await call("close", p.repo, p.number);
    toast(x.ok ? `${p.repo} #${p.number} closed` : `Close failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); };
  if (selectable) row.onclick = () => { state.selPR = p; renderApprovals($("#view")); };
  return row;
}
async function mergeAllGreen() {
  const green = allPRs().filter(p => p.checks === "success");
  if (!green.length) return toast("No green PRs to merge", "err");
  let n = 0; for (const p of green) { const x = await call("merge", p.repo, p.number); if (x.ok) n++; }
  toast(`Merged ${n}/${green.length} green PR${green.length === 1 ? "" : "s"}`, "ok"); refresh();
}

/* ---------- approvals full view ---------- */
function renderApprovals(v) {
  const prs = allPRs();
  v.innerHTML = `<p class="eyebrow">Review</p><h1 class="view-title">Approvals</h1>
    <p class="view-sub">${prs.length} open pull request${prs.length === 1 ? "" : "s"} awaiting your call</p>`;
  if (!state.gh_ready) { v.appendChild(el("div", "banner", `Run <code>gh auth login</code> to enable PR review.`)); return; }
  if (!prs.length) { v.appendChild(el("div", "empty", "No open rsi/* pull requests.")); return; }
  const grid = el("div", "appr-grid"); v.appendChild(grid);
  const list = el("div", "appr-list"); grid.appendChild(list);
  if (!state.selPR) state.selPR = prs[0];
  prs.forEach(p => { const row = prRow(p, true); if (state.selPR && p.repo === state.selPR.repo && p.number === state.selPR.number) row.classList.add("sel"); list.appendChild(row); });
  const diff = el("div", "diff", `<div class="diff-head">${esc(state.selPR.repo)} #${esc(state.selPR.number)} &middot; loading diff…</div><div class="diff-body"></div>`);
  grid.appendChild(diff);
  loadDiff(state.selPR, diff);
}
async function loadDiff(p, diffEl) {
  const key = p.repo + "#" + p.number;
  let res = state.diffCache[key];
  if (!res) { res = await call("pr_diff", p.repo, p.number); state.diffCache[key] = res; }
  const body = $(".diff-body", diffEl); $(".diff-head", diffEl).innerHTML = `${esc(p.repo)} #${esc(p.number)} &middot; <code>${esc(p.headRefName || "")}</code>`;
  if (!res || !res.ok) { body.innerHTML = `<div class="ln hh">${esc((res && res.error) || "diff unavailable")}</div>`; return; }
  const lines = (res.diff || "").split("\n").slice(0, 1200);
  body.innerHTML = lines.map(l => {
    let c = ""; if (l.startsWith("+") && !l.startsWith("+++")) c = "add";
    else if (l.startsWith("-") && !l.startsWith("---")) c = "del";
    else if (l.startsWith("@@") || l.startsWith("diff ") || l.startsWith("index ")) c = "hh";
    return `<div class="ln ${c}">${esc(l) || "&nbsp;"}</div>`;
  }).join("") + (res.truncated ? `<div class="ln hh">… diff truncated …</div>` : "");
}

/* ---------- history ---------- */
async function renderHistory(v) {
  if (!state.repos.length) { v.innerHTML = `<p class="eyebrow">Trace</p><h1 class="view-title">History</h1>`; v.appendChild(el("div", "empty", "No repositories yet.")); return; }
  if (!state.histRepo || !state.repos.some(r => r.name === state.histRepo)) state.histRepo = state.repos[0].name;
  v.innerHTML = `<p class="eyebrow">Trace</p><h1 class="view-title">History</h1>
    <p class="view-sub">Past iterations &mdash; shipped, reverted, or no-op &mdash; with their gate results.</p>
    <select class="select console-pick" id="histPick">${state.repos.map(r => `<option value="${esc(r.name)}"${r.name === state.histRepo ? " selected" : ""}>${esc(r.name)}</option>`).join("")}</select>
    <div class="trend" id="trend"><h4>Tests passed over time</h4><div id="spark"></div></div>
    <div class="timeline" id="tl"><div class="empty">Loading…</div></div>`;
  $("#histPick", v).onchange = (e) => { state.histRepo = e.target.value; renderHistory(v); };
  const [hist, m] = await Promise.all([call("read_history", state.histRepo), call("metrics", state.histRepo)]);
  $("#spark", v).innerHTML = sparkline((m && m.tests_series) || []);
  const tl = $("#tl", v);
  if (!hist || !hist.length) { tl.innerHTML = `<div class="empty">No iterations recorded yet.</div>`; return; }
  tl.innerHTML = "";
  hist.slice().reverse().forEach(rec => {
    const item = el("div", "tl");
    const t = rec.tests; const tstr = t ? `${t.passed || 0} passed${t.failed ? ", " + t.failed + " failed" : ""}` : "no gate";
    item.innerHTML = `<span class="tl-dot ${esc(rec.status)}"></span>
      <div class="tl-main"><div class="tl-status">${esc(rec.status)}</div>
        ${rec.summary ? `<div class="tl-sum">${esc(rec.summary)}</div>` : ""}
        <div class="tl-meta"><span>${esc(rec.branch || "")}</span><span>${esc(tstr)}</span><span>${esc(ago(rec.ts))}</span></div></div>`;
    tl.appendChild(item);
  });
}
function sparkline(series) {
  if (!series || series.length < 2) return `<div class="empty" style="padding:8px 0">Not enough data yet.</div>`;
  const vals = series.map(s => s.passed || 0); const max = Math.max(...vals, 1); const W = 520, H = 60;
  const pts = vals.map((y, i) => `${(i / (vals.length - 1)) * W},${H - (y / max) * (H - 6) - 3}`).join(" ");
  return `<svg viewBox="0 0 ${W} ${H}" width="100%" height="${H}" preserveAspectRatio="none"><polyline points="${pts}" fill="none" stroke="var(--clay)" stroke-width="2" stroke-linejoin="round"/></svg>`;
}

/* ---------- console ---------- */
async function renderConsole(v) {
  if (!state.repos.length) { v.innerHTML = `<p class="eyebrow">Stream</p><h1 class="view-title">Console</h1>`; v.appendChild(el("div", "empty", "No repositories yet.")); return; }
  if (!state.consoleRepo || !state.repos.some(r => r.name === state.consoleRepo)) state.consoleRepo = state.repos[0].name;
  v.innerHTML = `<p class="eyebrow">Stream</p><h1 class="view-title">Console</h1>
    <p class="view-sub">Live log tail from the improver runtime.</p>
    <select class="select console-pick" id="conPick">${state.repos.map(r => `<option value="${esc(r.name)}"${r.name === state.consoleRepo ? " selected" : ""}>${esc(r.name)}</option>`).join("")}</select>
    <div class="console" id="con"><div class="watermark">&#8734;</div></div>`;
  $("#conPick", v).onchange = (e) => { state.consoleRepo = e.target.value; renderConsole(v); };
  const r = await call("read_log", state.consoleRepo);
  const con = $("#con", v);
  const log = (r && r.log) || "";
  con.innerHTML = log ? log.split("\n").map(l => `<div class="cl">${esc(l) || "&nbsp;"}</div>`).join("")
    : `<div class="watermark">&#8734;</div><div class="empty">No activity logged yet.</div>`;
  con.scrollTop = con.scrollHeight;
}

/* ---------- settings ---------- */
function renderSettings(v) {
  const k = state.keys || {}; const gh = state.github || {};
  v.innerHTML = `<p class="eyebrow">Configure</p><h1 class="view-title">Settings</h1>
    <p class="view-sub">Keys, GitHub, and global behavior.</p>
    <div class="set-sec"><h4>API keys</h4><p>Stored locally in <code>.env</code> &mdash; never shown back.</p>
      <div class="set-row"><label>Ollama Cloud ${k["ollama-cloud"] ? "&#10003;" : "&mdash;"}</label><input class="input" id="kOllama" type="password" placeholder="OLLAMA_API_KEY" /><button class="btn sm accent" data-prov="ollama-cloud">Save</button></div>
      <div class="set-row"><label>OpenRouter ${k["openrouter"] ? "&#10003;" : "&mdash;"}</label><input class="input" id="kOpen" type="password" placeholder="OPENROUTER_API_KEY" /><button class="btn sm accent" data-prov="openrouter">Save</button></div>
    </div>
    <div class="set-sec"><h4>GitHub</h4><p>${gh.login ? `Connected as <b>${esc(gh.login)}</b>.` : `Run <code>gh auth login</code> to connect.`}</p>
      <div class="set-row"><label>Add project</label><input class="input" id="addRepo" type="text" placeholder="owner/repo or GitHub URL" /><button class="btn sm accent" id="addRepoBtn">Add</button></div>
    </div>
    <div class="set-sec"><h4>Global</h4><p>Auto-push gate is in the top bar. Maintenance below.</p>
      <div class="set-row"><label>AI fixes</label>
        <button class="switch ${state.auto_ai_fix ? "on" : ""}" id="aiFixToggle" title="Allow the unattended supervise sweep to run pi fix-sessions"><span class="knob"></span></button>
        <span style="font-size:11.5px;color:var(--ink-3)">let the unattended <code>--supervise</code> sweep auto-run a pi fix-session on a gate-red streak (the manual Supervise button still needs the per-repo tick)</span></div>
      <div class="set-row"><label>Worktrees</label><button class="btn sm" id="cleanAll">${icon("broom", 14)}Clean up all worktrees</button></div>
    </div>`;
  v.querySelectorAll("[data-prov]").forEach(b => b.onclick = async () => {
    const prov = b.dataset.prov; const inp = prov === "openrouter" ? $("#kOpen", v) : $("#kOllama", v);
    if (!inp.value) return toast("Enter a key first", "err");
    const x = await call("set_key", prov, inp.value);
    toast(x.ok ? `${PROVIDER_LABEL[prov]} key saved` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh();
  });
  $("#addRepoBtn", v).onclick = async () => {
    const spec = $("#addRepo", v).value.trim(); if (!spec) return;
    const x = await call("add_project", spec);
    toast(x.ok ? `Added ${x.name}${x.enriching ? " — enriching contract…" : ""}` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh();
  };
  const aft = $("#aiFixToggle", v);
  if (aft) aft.onclick = async () => {
    const n = !state.auto_ai_fix; state.auto_ai_fix = n; aft.classList.toggle("on", n);
    const x = await call("set_auto_ai_fix", n);
    if (!x || !x.ok) { state.auto_ai_fix = !n; aft.classList.toggle("on", !n); }
  };
  $("#cleanAll", v).onclick = () => confirmDialog("Clean up all worktrees?",
    "Prunes git worktrees and deletes leftover rsi/* iteration branches across every repo. The currently checked-out branch is never touched.",
    async () => {
      let total = 0; for (const r of state.repos) { const x = await call("cleanup_worktrees", r.name); if (x && x.ok) total += (x.removed || []).length; }
      toast(`Pruned ${total} stale branch${total === 1 ? "" : "es"}`, "ok"); refresh();
    });
}

/* ================= WORKSPACE ================= */
function openWorkspace(name) { state.openRepo = name; state.wsTab = "activity"; renderWorkspace(); }
function closeWorkspace() { state.openRepo = null; const s = $("#wsScrim"); if (s) s.remove(); }
function renderWorkspace() {
  const r = state.repos.find(x => x.name === state.openRepo); if (!r) return closeWorkspace();
  let scrim = $("#wsScrim");
  if (!scrim) { scrim = el("div", "ws-scrim"); scrim.id = "wsScrim"; document.body.appendChild(scrim);
    scrim.onclick = (e) => { if (e.target === scrim) closeWorkspace(); }; }
  const hb = r.heartbeat || {}; const si = statusInfo(r); const ds = diagState(r); const esc1 = r.escalation;
  const TABS = [["activity", "Activity"], ["diff", "Diff"], ["backlog", "Backlog"], ["contract", "Contract"], ["config", "Config"], ["supervisor", "Supervisor"]];
  scrim.innerHTML = `<div class="ws">
    <div class="ws-head"><div class="ws-title">&#8734; ${esc(r.name)} <span class="pill ${si.cls}">${esc(si.label)}</span>${ds.flagged ? ` <span class="pill ${ds.cls}">${esc(ds.label)}</span>` : ""}</div>
      <button class="btn sm ghost" id="wsClose">${icon("x", 15)}</button></div>
    <div class="ws-goal-wrap"><div class="ws-goal-k">Goal</div>
      <div class="ws-goal">${hb.goal ? esc(hb.goal) : (r.running ? "starting…" : "idle — press Start")}</div>
      <div class="ws-sub"><span>phase ${esc(hb.phase || "—")}</span><span>iter ${esc(hb.iteration ?? 0)}</span><span>model ${esc(hb.model || r.model || "—")}</span><span>reasoning ${esc(r.reasoning || "default")}</span></div></div>
    ${esc1 ? `<div class="esc-banner"><div class="esc-h">${icon("bolt", 15)} Escalated: ${esc(esc1.category)} — needs your action</div>
      <div class="esc-ev">${esc(esc1.evidence || "")}</div>
      <pre class="esc-steps">${esc((esc1.suggested_manual_steps || []).join("\n"))}</pre>
      <button class="btn sm ghost" id="escDismiss">Dismiss</button></div>` : ""}
    <div class="ws-tabs">${TABS.map(([k, l]) => `<button class="ws-tab${state.wsTab === k ? " active" : ""}" data-tab="${k}">${l}</button>`).join("")}</div>
    <div class="ws-body" id="wsBody"></div>
    <div class="ws-foot">
      <button class="btn sm ${r.running ? "danger" : "accent"}" id="wsToggle"${(!r.is_git && !r.running) ? " disabled" : ""}>${icon(r.running ? "stop" : "play", 14)}${r.running ? "Stop" : "Start"}</button>
      <button class="btn sm" id="wsOnce"${(!r.is_git || r.running) ? " disabled" : ""}>Run once</button>
      <button class="btn sm" id="wsBeautify"${!r.is_git ? " disabled" : ""}>${icon("sparkle", 14)}Beautify</button>
      <label class="ai-fix" title="allow a one-shot pi fix-session for a persistent gate failure"><input type="checkbox" id="wsAllowPi" /> Allow AI fix</label>
      <button class="btn sm" id="wsSupervise">${icon("bolt", 14)}Supervise</button>
      <div class="spacer"></div>
      <button class="btn sm ghost" id="wsClean" title="Prune stale worktrees & rsi/* branches">${icon("broom", 14)}Clean up worktrees</button>
    </div>
  </div>`;
  scrim.querySelectorAll(".ws-tab").forEach(t => t.onclick = () => { state.wsTab = t.dataset.tab; renderWsTab(r); });
  $("#wsClose").onclick = closeWorkspace;
  $("#wsToggle").onclick = async () => { const x = r.running ? await call("stop", r.name) : await call("start", r.name);
    toast(x.ok ? `${r.name}: ${r.running ? "stopping" : "started"}` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); };
  $("#wsOnce").onclick = async () => { const x = await call("start", r.name, true);
    toast(x.ok ? `${r.name}: running one iteration` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); };
  $("#wsBeautify").onclick = async () => { const x = await call("beautify", r.name);
    toast(x.ok ? `Beautifying ${r.name}…` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); };
  $("#wsClean").onclick = () => confirmDialog(`Clean up ${r.name} worktrees?`,
    "Prunes git worktrees and deletes leftover rsi/* iteration branches. The currently checked-out branch is never touched.",
    async () => { const x = await call("cleanup_worktrees", r.name);
      toast(x.ok ? `${r.name}: pruned ${(x.removed || []).length} stale branch${(x.removed || []).length === 1 ? "" : "es"}` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh(); });
  const esd = $("#escDismiss"); if (esd) esd.onclick = async () => { await call("clear_escalation", r.name); refresh(); if (state.openRepo) renderWorkspace(); };
  $("#wsSupervise").onclick = async () => {
    $("#wsSupervise").disabled = true;
    await doSupervise(r.name, $("#wsAllowPi") && $("#wsAllowPi").checked);
    if (state.openRepo) renderWorkspace();
  };
  renderWsTab(r);
}
async function renderWsTab(r) {
  scrimActiveTabs(); const body = $("#wsBody"); if (!body) return;
  const hb = r.heartbeat || {};
  if (state.wsTab === "activity") {
    body.innerHTML = `<div class="console" id="wsLog" style="height:auto;min-height:300px">loading…</div>`;
    const res = await call("read_log", r.name); const log = (res && res.log) || (hb.log_tail || []).join("\n");
    const con = $("#wsLog", body);
    con.innerHTML = log ? log.split("\n").map(l => `<div class="cl">${esc(l) || "&nbsp;"}</div>`).join("") : `<div class="empty">No activity yet.</div>`;
    con.scrollTop = con.scrollHeight; return;
  }
  if (state.wsTab === "diff") {
    const pr = hb.last_pr;
    if (!pr || !pr.number) { body.innerHTML = `<div class="empty">No open PR to diff.</div>`; return; }
    body.innerHTML = `<div class="diff"><div class="diff-head">loading diff…</div><div class="diff-body"></div></div>`;
    loadDiff({ repo: r.name, number: pr.number, headRefName: pr.branch }, $(".diff", body)); return;
  }
  if (state.wsTab === "backlog" || state.wsTab === "contract") {
    const which = state.wsTab === "backlog" ? "backlog" : "agent";
    body.innerHTML = `<p class="contract-note">${which === "agent"
      ? "<b>AGENT.md</b> — the agent contract. This is the one file you edit to steer the loop (goal, rules, what's off-limits)."
      : "<b>backlog.md</b> — the queue of improvements the agent pulls from, top first."}</p>
      <textarea class="textarea" id="wsEditor">loading…</textarea>
      <div style="margin-top:10px"><button class="btn sm accent" id="wsSaveDoc">Save ${which === "agent" ? "AGENT.md" : "backlog.md"}</button>
      <button class="btn sm" id="wsEnrich" title="Read the repo and rewrite this contract tailored to it (uses your API key)">${icon("sparkle", 14)}Enrich with AI</button></div>`;
    const res = await call("read_contract", r.name, which);
    $("#wsEditor", body).value = (res && res.text) || "";
    $("#wsSaveDoc", body).onclick = async () => { const x = await call("write_contract", r.name, which, $("#wsEditor", body).value);
      toast(x.ok ? "Saved" : `Failed: ${x.error}`, x.ok ? "ok" : "err"); };
    $("#wsEnrich", body).onclick = async () => {
      const b = $("#wsEnrich", body); b.disabled = true; b.textContent = "Enriching…";
      const x = await call("enrich_contract", r.name);
      toast(x && x.ok ? `${r.name}: contract enriched` : `Enrich failed: ${(x && x.error) || "?"}`, x && x.ok ? "ok" : "err");
      const res2 = await call("read_contract", r.name, which);
      if (res2 && typeof res2.text === "string") $("#wsEditor", body).value = res2.text;
      b.disabled = false; b.innerHTML = `${icon("sparkle", 14)}Enrich with AI`;
    };
    return;
  }
  if (state.wsTab === "config") {
    body.innerHTML = `<div class="cfg-grid">
      <div class="cfg-field"><label>Provider</label><select class="select" id="cP">${state.providers.map(p => `<option value="${p}"${p === (r.provider || "ollama-cloud") ? " selected" : ""}>${esc(PROVIDER_LABEL[p] || p)}</option>`).join("")}</select></div>
      <div class="cfg-field"><label>Model</label><input class="input" id="cM" value="${esc(r.model || "")}" placeholder="model id" /></div>
      <div class="cfg-field"><label>Ship mode</label><select class="select" id="cS">${SHIP_MODES.map(([vv, l]) => `<option value="${vv}"${vv === (r.ship || "pr") ? " selected" : ""}>${l}</option>`).join("")}</select></div>
      <div class="cfg-field"><label>PR target branch</label><input class="input" id="cB" value="${esc(r.pr_target_branch || "main")}" placeholder="main" /></div>
      <div class="cfg-field"><label>Reasoning level</label><select class="select" id="cR">${REASONING.map(rv => `<option value="${rv}"${rv === (r.reasoning || "") ? " selected" : ""}>${rv === "" ? "Default" : rv}</option>`).join("")}</select></div>
      <div class="cfg-field"><label>Gate command</label><input class="input" id="cG" value="${esc(r.gate || "")}" placeholder="pytest (default)" /></div>
      <div class="cfg-field"><label>Interval (s)</label><input class="input" id="cI" type="number" value="${esc(r.interval ?? 120)}" /></div>
      <div class="cfg-field"><label>Max iterations</label><input class="input" id="cX" type="number" value="${esc(r.max_iterations ?? 0)}" /></div>
      <div class="cfg-field full"><button class="btn accent" id="cSave">Save configuration</button></div>
    </div>`;
    $("#cSave", body).onclick = async () => {
      const x = await call("set_repo_config", r.name, $("#cP", body).value, $("#cM", body).value.trim(),
        $("#cS", body).value, $("#cG", body).value.trim(), $("#cB", body).value.trim() || "main",
        parseInt($("#cI", body).value) || 120, parseInt($("#cX", body).value) || 0, $("#cR", body).value);
      toast(x.ok ? `${r.name}: configuration saved` : `Failed: ${x.error}`, x.ok ? "ok" : "err"); refresh();
    };
    return;
  }
  if (state.wsTab === "supervisor") {
    body.innerHTML = `<p class="contract-note">Supervisor activity for ${esc(r.name)} — automatic safe recoveries and escalations.</p>
      <div class="console" id="supLog" style="height:auto;min-height:240px">loading…</div>`;
    const recs = await call("read_supervisor_log", r.name);
    const con = $("#supLog", body);
    con.innerHTML = (recs && recs.length)
      ? recs.slice().reverse().map(s => `<div class="cl">${esc(s.ts || "")} · ${esc(s.category || "")} · ${s.escalate ? "ESCALATED" : "rung " + (s.rung ?? 0)}${(s.actions || []).length ? " · " + esc((s.actions || []).join(", ")) : ""}${s.message ? " — " + esc(s.message) : ""}</div>`).join("")
      : `<div class="empty">No supervisor activity yet.</div>`;
    return;
  }
}
function scrimActiveTabs() { const s = $("#wsScrim"); if (!s) return; s.querySelectorAll(".ws-tab").forEach(t => t.classList.toggle("active", t.dataset.tab === state.wsTab)); }

/* ---------- confirm + palette ---------- */
function confirmDialog(title, msg, onYes) {
  const scrim = el("div", "confirm-scrim");
  scrim.innerHTML = `<div class="confirm"><h4>${esc(title)}</h4><p>${esc(msg)}</p>
    <div class="confirm-acts"><button class="btn sm ghost" id="cNo">Cancel</button><button class="btn sm accent" id="cYes">Confirm</button></div></div>`;
  document.body.appendChild(scrim);
  scrim.onclick = (e) => { if (e.target === scrim) scrim.remove(); };
  $("#cNo", scrim).onclick = () => scrim.remove();
  $("#cYes", scrim).onclick = () => { scrim.remove(); onYes(); };
}
function openPalette() {
  if ($("#palScrim")) return;
  const scrim = el("div", "palette-scrim"); scrim.id = "palScrim";
  const actions = [];
  NAV.forEach(([v, l]) => actions.push({ label: "Go to " + l, run: () => setView(v) }));
  state.repos.forEach(r => {
    actions.push({ label: `Open ${r.name}`, hint: "workspace", run: () => openWorkspace(r.name) });
    actions.push({ label: `${r.running ? "Stop" : "Start"} ${r.name}`, run: async () => { await call(r.running ? "stop" : "start", r.name); refresh(); } });
    actions.push({ label: `Run once: ${r.name}`, run: async () => { await call("start", r.name, true); refresh(); } });
    actions.push({ label: `Clean up worktrees: ${r.name}`, run: async () => { const x = await call("cleanup_worktrees", r.name); toast(x.ok ? `${r.name}: pruned ${(x.removed || []).length}` : x.error, x.ok ? "ok" : "err"); } });
  });
  scrim.innerHTML = `<div class="palette"><input id="palIn" placeholder="Search repos and actions…" autocomplete="off" /><div class="palette-list" id="palList"></div></div>`;
  document.body.appendChild(scrim);
  scrim.onclick = (e) => { if (e.target === scrim) scrim.remove(); };
  const inp = $("#palIn", scrim); const list = $("#palList", scrim); let sel = 0, shown = actions;
  const draw = () => { list.innerHTML = ""; shown.forEach((a, i) => { const it = el("div", "palette-item" + (i === sel ? " sel" : ""), `<span>${esc(a.label)}</span>${a.hint ? `<span class="pk">${esc(a.hint)}</span>` : ""}`); it.onclick = () => { scrim.remove(); a.run(); }; list.appendChild(it); }); };
  inp.oninput = () => { const q = inp.value.toLowerCase(); shown = actions.filter(a => a.label.toLowerCase().includes(q)); sel = 0; draw(); };
  inp.onkeydown = (e) => { if (e.key === "ArrowDown") { sel = Math.min(sel + 1, shown.length - 1); draw(); e.preventDefault(); }
    else if (e.key === "ArrowUp") { sel = Math.max(sel - 1, 0); draw(); e.preventDefault(); }
    else if (e.key === "Enter") { if (shown[sel]) { scrim.remove(); shown[sel].run(); } }
    else if (e.key === "Escape") scrim.remove(); };
  draw(); inp.focus();
}

/* ---------- refresh / poll ---------- */
function busy() {
  const ae = document.activeElement; if (ae && /INPUT|TEXTAREA|SELECT/.test(ae.tagName)) return true;
  return !!($("#palScrim") || $(".confirm-scrim"));
}
async function refresh() {
  try {
    const s = await call("get_state");
    state.repos = s.repos || []; state.gh_ready = !!s.gh_ready; state.keys = s.keys || {};
    state.github = s.github || {}; if (s.providers) state.providers = s.providers;
    if (typeof s.auto_push === "boolean") state.auto_push = s.auto_push;
    if (typeof s.auto_ai_fix === "boolean") state.auto_ai_fix = s.auto_ai_fix;
    if (s.theme && s.theme !== state.theme) applyTheme(s.theme);
    syncTopbar(); syncRail();
    if (busy()) return;                       // don't clobber open editors / palette
    if (state.openRepo && state.wsTab !== "activity") { return; }  // keep workspace editors stable
    if (state.openRepo) { renderWorkspace(); return; }
    renderView(); renderDock();
  } catch (e) { /* backend not ready */ }
}
let _poll = false;
async function poll() { await refresh(); setTimeout(poll, 2500); }

/* ---------- boot ---------- */
let _tries = 0;
async function boot() {
  buildThemeRow(); buildRail();
  $("#autoPush").onclick = async () => { const next = !state.auto_push; state.auto_push = next; syncTopbar();
    const x = await call("set_auto_push", next); if (!x || !x.ok) { state.auto_push = !next; syncTopbar(); } };
  $("#refreshBtn").onclick = () => { refresh(); toast("Refreshed"); };
  document.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openPalette(); }
    if (e.key === "Escape") { if ($("#palScrim")) $("#palScrim").remove(); else if (state.openRepo) closeWorkspace(); }
  });
  try { await call("get_state"); } catch (e) { if (++_tries < 100) return setTimeout(boot, 250); }
  applyTheme(state.theme);
  if (!_poll) { _poll = true; poll(); }
}
if (MOCK || realApi()) boot(); else window.addEventListener("pywebviewready", boot);
setTimeout(() => { if (!_poll) boot(); }, 800);

/* ================= MOCK (browser preview only) ================= */
const mock = (() => {
  const repos = [
    { name: "maki", path: "C:/p/maki", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "pr",
      pr_target_branch: "main", reasoning: "high", interval: 120, max_iterations: 0, gate: null, is_git: true, has_remote: true, running: true,
      heartbeat: { status: "iterating", phase: "implement", iteration: 47, goal: "Add OAuth2 refresh-token rotation to the auth service",
        model: "kimi-k2.7-code", tests: { passed: 218, failed: 0 }, last_pr: { number: 318, branch: "rsi/iter-x", state: "open" },
        log_tail: ["12:01:03Z iteration 47: branch rsi/iter-x — Pi working", "12:03:21Z gate: GREEN {passed:218}", "12:03:30Z opened PR #318"] },
      prs: [{ number: 318, title: "Rotate refresh tokens on reuse detection", headRefName: "rsi/feat/token-rotation", checks: "success", url: "#" },
            { number: 319, title: "Add rate-limit headers to auth endpoints", headRefName: "rsi/feat/rate-limit", checks: "success", url: "#" }],
      contracts: { agent: true, backlog: true }, diagnosis: { category: "ok", healthy: true, evidence: "iterating" }, escalation: null },
    { name: "asmodeus", path: "C:/p/asmodeus", provider: "openrouter", model: "qwen/qwen3-coder", ship: "auto-merge",
      pr_target_branch: "main", reasoning: "medium", interval: 120, max_iterations: 0, gate: null, is_git: true, has_remote: true, running: false,
      heartbeat: { status: "sleeping", iteration: 12, goal: "Migrate the queue worker pool to async batches", tests: { passed: 89, failed: 2 }, last_pr: null, log_tail: [] },
      prs: [{ number: 284, title: "Async batch dispatch for queue workers", headRefName: "rsi/feat/async-batches", checks: "pending", url: "#" }],
      contracts: { agent: true, backlog: true }, diagnosis: { category: "gate_red_streak", healthy: false, auto_safe: false, evidence: "last 3 iterations reverted — the gate keeps failing" }, escalation: null },
    { name: "sover", path: "C:/p/sover", provider: "ollama-cloud", model: "kimi-k2.7-code", ship: "pr",
      pr_target_branch: "main", reasoning: "low", interval: 120, max_iterations: 0, gate: null, is_git: true, has_remote: true, running: false,
      heartbeat: { status: "stopped", iteration: 0, goal: null, tests: null, last_pr: null, log_tail: [] },
      prs: [{ number: 97, title: "Reconcile partial refunds in billing cron", headRefName: "rsi/fix/refund-recon", checks: "success", url: "#" }],
      contracts: { agent: true, backlog: true },
      diagnosis: { category: "revert_failed", healthy: false, auto_safe: false, evidence: "REVERT FAILED — base needs manual cleanup" },
      escalation: { category: "revert_failed", evidence: "REVERT FAILED — base needs manual cleanup",
        suggested_manual_steps: ['cd "C:/p/sover"', "git checkout --force main", "git reset --hard", "git reset --hard origin/main", "git status"] } },
  ];
  const hist = [
    { ts: "2026-06-17T10:00:00Z", status: "shipped", branch: "rsi/iter-a", tests: { passed: 200, failed: 0 }, summary: "Added token bucket limiter.", pr: { state: "merged" } },
    { ts: "2026-06-17T10:30:00Z", status: "reverted", branch: "rsi/iter-b", tests: { passed: 198, failed: 4 }, summary: "Reverted — tests failed.", pr: null },
    { ts: "2026-06-17T11:00:00Z", status: "noop", branch: "rsi/iter-c", tests: null, summary: "No change.", pr: null },
    { ts: "2026-06-17T11:30:00Z", status: "shipped", branch: "rsi/iter-d", tests: { passed: 210, failed: 0 }, summary: "Refactored session store.", pr: { state: "open" } },
    { ts: "2026-06-17T12:00:00Z", status: "shipped", branch: "rsi/iter-e", tests: { passed: 218, failed: 0 }, summary: "Refresh-token rotation.", pr: { state: "open" } },
  ];
  const find = n => repos.find(r => r.name === n);
  return {
    get_state: () => ({ repos, gh_ready: true, theme: state.theme, auto_push: state.auto_push, auto_ai_fix: state.auto_ai_fix,
      providers: ["ollama-cloud", "openrouter"], keys: { "ollama-cloud": true, "openrouter": false }, github: { ready: true, login: "cayleb" } }),
    set_theme: (t) => ({ ok: true, theme: t }),
    set_auto_push: (v) => { state.auto_push = !!v; return { ok: true, auto_push: !!v }; },
    set_auto_ai_fix: (v) => { state.auto_ai_fix = !!v; return { ok: true, auto_ai_fix: !!v }; },
    start: (n) => { const r = find(n); if (r) { r.running = true; r.heartbeat.status = "iterating"; } return { ok: true }; },
    stop: (n) => { const r = find(n); if (r) { r.running = false; r.heartbeat.status = "stopped"; } return { ok: true }; },
    beautify: () => ({ ok: true }),
    merge: () => ({ ok: true }), close: () => ({ ok: true }),
    set_repo_config: () => ({ ok: true }), set_key: () => ({ ok: true }), add_project: (s) => ({ ok: true, name: s.split("/").pop(), enriching: true }),
    publish: () => ({ ok: true }),
    pr_diff: () => ({ ok: true, diff: "diff --git a/auth.py b/auth.py\n@@ -10,6 +10,9 @@ def rotate():\n-    return token\n+    new = mint(token)\n+    revoke(token)\n+    return new", truncated: false }),
    read_log: (n) => ({ ok: true, log: ((find(n) || {}).heartbeat || {}).log_tail?.join("\n") || "12:00:01Z improver started\n12:00:02Z idle" }),
    read_history: () => hist,
    read_contract: (n, w) => ({ ok: true, text: w === "agent" ? "# AGENT.md\n\nGoal: keep improving this repo. Make ONE small change per iteration, add a test, never touch git.\n\nOff-limits: the framework, CI config." : "- [ ] Add OAuth2 refresh-token rotation\n- [ ] Rate-limit auth endpoints" }),
    write_contract: () => ({ ok: true }),
    metrics: () => ({ iterations: 5, shipped: 3, merged: 1, reverted: 1, noop: 1, success_rate: 0.6, tests_series: hist.filter(h => h.tests).map(h => ({ ts: h.ts, passed: h.tests.passed, failed: h.tests.failed })) }),
    health: () => ({ gh: true, git: true, keys: { "ollama-cloud": true, "openrouter": false }, repos: repos.map(r => ({ name: r.name, is_git: true, has_remote: true, venv: true })) }),
    cleanup_worktrees: () => ({ ok: true, pruned: true, removed: ["rsi/iter-old1", "rsi/iter-old2", "rsi/iter-old3"] }),
    ensure_contracts: () => ({ ok: true, created: [] }),
    enrich_contract: (n) => ({ ok: true, agent_written: 1400, backlog_written: 320, summary: "# " + n + " self-improvement contract" }),
    supervise: (n, allowPi) => {
      const cat = ((find(n) || {}).diagnosis || {}).category || "ok";
      if (cat === "revert_failed") return { ok: true, results: [{ name: n, category: cat, actions_taken: ["reset_to_base"], escalate: true, message: "un-pushed commits on main — escalate" }] };
      if (cat === "gate_red_streak") return { ok: true, results: [{ name: n, category: cat, actions_taken: allowPi ? ["solomon_fix_session"] : [], escalate: !allowPi, message: allowPi ? "launched Solomon fix-session" : "tick 'Allow AI fix' to run a fix-session" }] };
      return { ok: true, results: [{ name: n, category: cat, actions_taken: ["clear_lock"], escalate: false, message: "recovered" }] };
    },
    read_supervisor_log: () => [{ ts: "2026-06-17T12:05:00Z", category: "stale_lock", rung: 0, actions: ["clear_lock"], escalate: false, message: "recovered" }],
    read_escalation: (n) => (find(n) || {}).escalation || null,
    clear_escalation: (n) => { const r = find(n); if (r) r.escalation = null; return { ok: true }; },
  };
})();
