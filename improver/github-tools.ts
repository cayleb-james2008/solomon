// GitHub tools for the Solomon RSI agent (pi / @mariozechner/pi-coding-agent).
//
// pi has no native MCP client, so this extension registers READ-ONLY GitHub tools
// (backed by the already-authenticated `gh` CLI) that the agent can call to verify its
// work landed and to read CI status. The runner (run_improver.py) loads this with -e
// ONLY for repos that have a GitHub remote, and gates such repos on GitHub connectivity
// before iterating. The agent must still NOT run git/gh directly, push, or merge — the
// runner ships the branch and opens the PR.
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { Type } from "@sinclair/typebox";

function run(bin: string, args: string[]) {
  // 30s timeout so a hung gh/git network call (e.g. ls-remote against an unreachable origin) fails fast
  // instead of blocking the pi process for the whole iteration budget. On timeout r.error is ETIMEDOUT,
  // surfaced via the err field below (and r.status is null -> code 1).
  const r = spawnSync(bin, args, { cwd: process.cwd(), encoding: "utf8", windowsHide: true, timeout: 30000 });
  return { code: r.status ?? 1, out: r.stdout || "", err: r.stderr || (r.error ? String(r.error) : "") };
}
function gh(args: string[]) {
  let r = run("gh", args);
  if (r.code !== 0 && /ENOENT|not found|not recognized/i.test(r.err)) {
    const fb = "C:\\Program Files\\GitHub CLI\\gh.exe";
    if (existsSync(fb)) r = run(fb, args);
  }
  return r;
}
function git(args: string[]) { return run("git", args); }
function curBranch() { return git(["rev-parse", "--abbrev-ref", "HEAD"]).out.trim() || "HEAD"; }
function clip(s: string, n = 240) { return (s || "").trim().slice(0, n); }

function summarizeChecks(rollup: any): string {
  if (!rollup || !rollup.length) return "none";
  let bad = 0, pend = 0, ok = 0;
  for (const c of rollup) {
    const st = (c.state || "").toUpperCase();
    const status = (c.status || "").toUpperCase();
    const concl = (c.conclusion || "").toUpperCase();
    if (["FAILURE", "ERROR"].includes(st) ||
        ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"].includes(concl)) bad++;
    else if ((status && status !== "COMPLETED") || st === "PENDING") pend++;
    else ok++;
  }
  return bad ? `FAILING (${bad} failed, ${ok} ok)` : pend ? `pending (${pend})` : `green (${ok})`;
}
const text = (t: string) => ({ content: [{ type: "text", text: t }] });

export default function (pi: any) {
  pi.registerTool({
    name: "github_status",
    label: "GitHub status",
    description: "Check the GitHub connection and the current repo's remote (auth, nameWithOwner, url, default branch). Read-only.",
    promptSnippet: "Check GitHub connection + current repo remote",
    parameters: Type.Object({}),
    async execute() {
      const auth = gh(["auth", "status"]);
      const view = gh(["repo", "view", "--json", "nameWithOwner,url,defaultBranchRef"]);
      return text(
        `gh auth: ${auth.code === 0 ? "CONNECTED" : "NOT CONNECTED"}\n` +
        `${clip(auth.err || auth.out, 300)}\n` +
        `repo: ${view.code === 0 ? clip(view.out, 400) : "(unavailable) " + clip(view.err)}`);
    },
  });

  pi.registerTool({
    name: "github_verify_push",
    label: "Verify branch pushed",
    description: "Confirm a branch exists on the origin remote (i.e. a push actually landed). Defaults to the current branch. Read-only.",
    promptSnippet: "Confirm a branch is on origin",
    parameters: Type.Object({ branch: Type.Optional(Type.String({ description: "branch name; default = current branch" })) }),
    async execute(_id: string, params: any) {
      const b = (params && params.branch) || curBranch();
      const r = git(["ls-remote", "--heads", "origin", b]);
      const found = r.code === 0 && r.out.includes("refs/heads/" + b);
      const sha = found ? r.out.trim().split(/\s+/)[0] : "";
      return text(found ? `OK: '${b}' is on origin (${sha.slice(0, 12)})`
                        : `NOT FOUND on origin: '${b}'\n${clip(r.err || r.out)}`);
    },
  });

  pi.registerTool({
    name: "github_pr_status",
    label: "PR status",
    description: "Show the open PR for a branch — state, mergeability, and CI check rollup. Defaults to the current branch. Read-only.",
    promptSnippet: "PR + CI status for a branch",
    parameters: Type.Object({ branch: Type.Optional(Type.String({ description: "branch name; default = current branch" })) }),
    async execute(_id: string, params: any) {
      const b = (params && params.branch) || curBranch();
      const r = gh(["pr", "view", b, "--json", "number,state,url,mergeStateStatus,statusCheckRollup"]);
      if (r.code !== 0) return text(`No PR for '${b}': ${clip(r.err || r.out)}`);
      let d: any;
      try { d = JSON.parse(r.out); } catch { return text(`github_pr_status: could not parse gh output: ${clip(r.out)}`); }
      return { content: [{ type: "text", text:
        `PR #${d.number} [${d.state}] ${d.url}\nmergeable: ${d.mergeStateStatus}\nCI: ${summarizeChecks(d.statusCheckRollup)}` }],
        details: d };
    },
  });

  pi.registerTool({
    name: "github_ci_status",
    label: "CI runs",
    description: "List recent GitHub Actions runs for a branch (workflow, status, conclusion, url). Defaults to the current branch. Read-only.",
    promptSnippet: "Recent CI runs for a branch",
    parameters: Type.Object({ branch: Type.Optional(Type.String({ description: "branch name; default = current branch" })) }),
    async execute(_id: string, params: any) {
      const b = (params && params.branch) || curBranch();
      const r = gh(["run", "list", "--branch", b, "--limit", "5", "--json", "workflowName,status,conclusion,headBranch,url"]);
      if (r.code !== 0) return text(`No CI runs for '${b}': ${clip(r.err || r.out)}`);
      return text(clip(r.out, 1500) || "(no runs)");
    },
  });

  pi.registerTool({
    name: "github_list_prs",
    label: "List open PRs",
    description: "List open pull requests in the current repo with their CI check rollup. Read-only.",
    promptSnippet: "List open PRs + CI",
    parameters: Type.Object({}),
    async execute() {
      const r = gh(["pr", "list", "--state", "open", "--json", "number,title,headRefName,statusCheckRollup"]);
      if (r.code !== 0) return text(`gh pr list failed: ${clip(r.err || r.out)}`);
      let arr: any[];
      try { arr = JSON.parse(r.out); } catch { return text(`github_list_prs: could not parse gh output: ${clip(r.out)}`); }
      const lines = arr.map((p) => `#${p.number} ${p.title} [${p.headRefName}] CI:${summarizeChecks(p.statusCheckRollup)}`);
      return text(lines.join("\n") || "(no open PRs)");
    },
  });
}
