//! The `Ctx` struct: run_improver.py's module-level GLOBALS, gathered into one value threaded as
//! `&mut Ctx` / `&Ctx` through the loop. Plus the setup/IO methods (configure, registry refresh,
//! per-phase config, env scrub, redaction, env load, logging, heartbeat, history, git/gh) and the
//! free time helpers `now()` / `stamp()`.
//!
//! Bug-for-bug with improver/run_improver.py. Where the Python globals are named-with-leading-`_`
//! (private), the Rust field/fn drops the underscore but keeps the semantics. Heartbeat key order is
//! preserved (crate enables serde_json `preserve_order`), so `heartbeat.json` matches the Python
//! `json.dumps(_hb, indent=2)` shape the dashboard reads.

use crate::control::{paths, proc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use chrono::Utc;

// --------------------------------------------------------------------------- #
// static tables / constants (run_improver.py module level)
// --------------------------------------------------------------------------- #

/// run_improver.GATE_TIMEOUT — seconds before a hung gate is force-failed. Raised to 3600s (60 min)
/// to match the implement timeout, so a cold `cargo test` on a large Rust workspace has the full hour
/// to compile + run rather than falsely RED-ing a good change at the 30-min mark.
pub const GATE_TIMEOUT: i64 = 3600;
/// run_improver._ESCALATE_TO_FALLBACK — cumulative failures before switching to the fallback model.
pub const ESCALATE_TO_FALLBACK: i64 = 2;
/// run_improver.DECOMPOSE_ENABLED — rung-2 goal decomposition (opt-in; off => the final rung defers).
pub const DECOMPOSE_ENABLED: bool = false;

/// One entry of run_improver.PROVIDERS: the shared pi extension file, the pi provider registration
/// name, and the provider's default model.
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    pub ext: &'static str,
    pub pi_provider: &'static str,
    pub default_model: &'static str,
}

/// run_improver.PROVIDERS — provider name -> {ext, pi_provider, default_model}. Both providers share
/// ONE parameterized pi extension (`provider.ts`), registered as `pi_provider` via RSI_PROVIDER.
pub fn providers(name: &str) -> Option<ProviderInfo> {
    match name {
        "ollama-cloud" => Some(ProviderInfo {
            ext: "provider.ts",
            pi_provider: "maki-cloud",
            default_model: "glm-5.2",
        }),
        "openrouter" => Some(ProviderInfo {
            ext: "provider.ts",
            pi_provider: "openrouter",
            default_model: "qwen/qwen3-coder",
        }),
        _ => None,
    }
}

/// run_improver.PROVIDERS.get(p) or PROVIDERS['ollama-cloud'] — the `... or PROVIDERS['ollama-cloud']`
/// fallback used by configure()/_refresh/_apply_phase_config.
fn providers_or_default(name: &str) -> ProviderInfo {
    providers(name).unwrap_or_else(|| providers("ollama-cloud").expect("ollama-cloud is defined"))
}

/// run_improver._FALLBACK_MODEL — provider NAME -> the stronger/different model the escalation ladder
/// switches to (rung 1). Keyed by PROVIDER_NAME, not pi_provider.
pub fn fallback_model(provider_name: &str) -> Option<&'static str> {
    match provider_name {
        "ollama-cloud" => Some("kimi-k2.7-code"),
        "openrouter" => Some("z-ai/glm-4.6"),
        _ => None,
    }
}

/// run_improver._CHEAP_MODEL — provider NAME -> the cheap worker model the light phases (beautify/e2e)
/// drop to.
pub fn cheap_model(provider_name: &str) -> Option<&'static str> {
    match provider_name {
        "ollama-cloud" => Some("minimax-m3"),
        "openrouter" => Some("qwen/qwen3-coder"),
        _ => None,
    }
}

/// run_improver._PHASE_DEFAULTS smart per-phase default: the light phases (beautify/e2e) carry
/// reasoning="low" + cheap=true; deep phases have no default (keep the repo's strong model/reasoning).
fn phase_default(phase: &str) -> Option<(&'static str, bool)> {
    match phase {
        // (reasoning, cheap)
        "beautify" => Some(("low", true)),
        "e2e" => Some(("low", true)),
        _ => None,
    }
}

/// run_improver._SECRET_TOKEN_PATTERNS — secret-shaped strings scrubbed from agent free-text before
/// it reaches a commit / PR body / history.jsonl / log. Compiled once (process-global cache).
fn secret_token_patterns() -> &'static [regex::Regex] {
    static PATS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            regex::Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap(), // GitHub PAT/OAuth/server/refresh
            regex::Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b").unwrap(), // fine-grained PAT
            regex::Regex::new(r"\bsk-[A-Za-z0-9_-]{20,}\b").unwrap(),      // OpenAI/Anthropic-style keys
            regex::Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}").unwrap(), // Authorization: Bearer <tok>
        ]
    })
}

/// run_improver._SECRET_KEYVAL_PATTERN — NAME<sep>value where NAME looks like a credential and value
/// is secret-length. Captures the separator (group 2) so legitimate text is not punctuation-rewritten.
/// Compiled once (process-global cache).
fn secret_keyval_pattern() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b([A-Za-z0-9_]*(?:API_?KEY|ACCESS_TOKEN|AUTH_TOKEN|SECRET|PASSWORD|TOKEN))\b(\s*[=:]\s*)([A-Za-z0-9_\-\.]{8,})",
        )
        .unwrap()
    })
}

/// run_improver._ENV_KEYS — provider API keys (+ OLLAMA_BASE_URL) loaded from Solomon/.env.
const ENV_KEYS: &[&str] = &["OLLAMA_API_KEY", "OLLAMA_BASE_URL", "OPENROUTER_API_KEY"];

// --------------------------------------------------------------------------- #
// time helpers (run_improver._now / _stamp)
// --------------------------------------------------------------------------- #

/// run_improver._now: the heartbeat/log timestamp `%Y-%m-%dT%H:%M:%SZ` in UTC.
pub fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// run_improver._stamp: a filesystem-safe UTC stamp `%Y%m%dT%H%M%SZ` (branch/file names).
pub fn stamp() -> String {
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// A 32-hex-char run-id matching the SHAPE of Python's `uuid.uuid4().hex` (run_improver.RUN_ID).
/// DEVIATION: NOT a cryptographic UUIDv4 (the `uuid` crate is not a declared dependency); uniqueness
/// comes from a splitmix64 stream seeded by the high-res clock, PID, and a per-process atomic counter
/// — the run-id is only compared verbatim by the single-flight lock, never parsed as a UUID. Mirrors
/// control::locks::rand_hex32 (which is private to that module).
fn run_id_hex() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = nanos
        ^ ((std::process::id() as u64) << 32)
        ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut next = || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    format!("{:016x}{:016x}", next(), next())
}

// --------------------------------------------------------------------------- #
// Ctx
// --------------------------------------------------------------------------- #

/// run_improver.py's module-level globals, gathered. `HERE` = Solomon/improver dir
/// (`paths::here().join("improver")`); `CONTROL` = Solomon dir (`paths::here()`). Both are resolved
/// from the foundation `paths::here()` at construction and cached on the struct so methods don't
/// re-derive them.
pub struct Ctx {
    // ---- HERE / CONTROL anchors (HERE=Solomon/improver, CONTROL=Solomon) ----
    pub here: PathBuf,
    pub control: PathBuf,

    // ---- per-run target + runtime paths (configure) ----
    pub repo: PathBuf,
    pub name: String,
    pub runtime: PathBuf,
    pub heartbeat_path: PathBuf,
    pub lock_path: PathBuf,
    pub stop_path: PathBuf,
    pub log_path: PathBuf,
    pub agent_md: PathBuf,
    pub backlog: PathBuf,
    pub lessons: PathBuf,
    pub venv_py: PathBuf,

    // ---- provider ----
    pub provider_name: String,
    pub pi_provider: String,
    pub pi_model: String,
    pub pi_ext: PathBuf,
    /// Per-repo API key (from repos.json `api_key`), overriding the global .env key for this repo's
    /// iterations. Empty when unset -> the global .env key (if any) is used. Applied to the process
    /// env in load_env() so run_pi/redact/required_key all read it through the existing env-var channel.
    pub api_key: String,

    // ---- HERE/*.md contracts the later phases need ----
    pub beautify_md: PathBuf,
    pub github_tools_ext: PathBuf,
    pub provision_md: PathBuf,
    pub ideate_md: PathBuf,
    pub reflect_md: PathBuf,
    pub solomon_md: PathBuf,
    pub review_md: PathBuf,
    pub plan_md: PathBuf,

    // ---- run config ----
    pub run_id: String,
    pub interval: i64,
    pub ship: String,
    pub gate_cmd: String,
    pub reasoning: String,
    pub goal: String,
    pub beautify: bool,
    pub solomon: bool,
    pub github_tools: bool,
    pub base_branch: String,
    pub phase: String,

    // ---- pipeline toggles ----
    pub review_enabled: bool,
    pub plan_enabled: bool,
    pub ideate_enabled: bool,
    pub reflect_enabled: bool,
    pub visual_review_enabled: bool,
    pub sandbox_config: Value, // None -> Null
    pub vision_model: String,

    // ---- mutable state ----
    pub hb: Value, // the _hb heartbeat OBJECT
    pub halted: bool,
    pub dbp_self_stop: bool,
    pub last_gate_feedback: String,
    pub last_visual_feedback: String,

    // ---- persistent-bail counters ----
    pub dirty_base_bail_count: i64,
    pub unpushed_base_bail_count: i64,
    pub base_gate_red_bail_count: i64,

    // ---- escalation ladder ----
    pub fail_counts: HashMap<String, i64>,
    pub escalated_goals: HashSet<String>,
}

impl Ctx {
    /// run_improver.configure (~146-171): point the runner at a target repo with a chosen
    /// provider/model. Resolves runtime/contract paths under Solomon (never inside the target repo),
    /// falls back to ollama-cloud for an unknown provider, and seeds `hb["repo"]`/`hb["model"]`. All
    /// other fields take run_improver's documented module-level defaults.
    pub fn configure(repo: &str, name: &str, provider: &str, model: Option<&str>) -> Ctx {
        let here = paths::here().join("improver"); // HERE = Solomon/improver
        let control = paths::here().to_path_buf(); // CONTROL = Solomon

        // REPO = Path(repo).resolve(): canonicalize when it exists; else best-effort absolute.
        let repo_path = resolve_path(repo);

        let runtime = control.join("runtime").join(name);
        let heartbeat_path = runtime.join("heartbeat.json");
        let lock_path = runtime.join("lock");
        let stop_path = runtime.join("stop");
        let log_path = runtime.join("improver.log");
        let agent_md = here.join(name).join("AGENT.md");
        let backlog = here.join(name).join("backlog.md");
        let lessons = here.join(name).join("LESSONS.md");
        // VENV_PY: Windows -> .venv/Scripts/python.exe; Unix -> .venv/Scripts/python.
        let venv_py = repo_path
            .join(".venv")
            .join("Scripts")
            .join(if cfg!(windows) { "python.exe" } else { "python" });

        // prov = PROVIDERS.get(provider) or PROVIDERS['ollama-cloud']
        let prov = providers_or_default(provider);
        // PROVIDER_NAME = provider if provider in PROVIDERS else 'ollama-cloud'
        let provider_name = if providers(provider).is_some() {
            provider.to_string()
        } else {
            "ollama-cloud".to_string()
        };
        let pi_provider = prov.pi_provider.to_string();
        // PI_MODEL = model or prov['default_model']  (Python truthiness: empty model string -> default)
        let pi_model = match model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => prov.default_model.to_string(),
        };
        let pi_ext = here.join(prov.ext);

        let run_id = run_id_hex();

        // _hb seed (run_improver module-level _hb dict). Built in Python insertion order so the
        // persisted heartbeat.json key order matches json.dumps(_hb, indent=2). pid is filled with the
        // host process id (this native binary), mirroring os.getpid().
        let mut hb = Map::new();
        hb.insert("repo".to_string(), json!(name)); // _hb["repo"] = name (configure overwrites the "maki" seed)
        hb.insert("status".to_string(), json!("starting"));
        hb.insert("phase".to_string(), Value::Null);
        hb.insert("pid".to_string(), json!(std::process::id()));
        hb.insert("run_id".to_string(), json!(run_id));
        hb.insert("iteration".to_string(), json!(0));
        hb.insert("goal".to_string(), Value::Null);
        hb.insert("model".to_string(), json!(pi_model)); // _hb["model"] = PI_MODEL
        hb.insert("tests".to_string(), Value::Null);
        hb.insert("last_pr".to_string(), Value::Null);
        hb.insert("last_summary".to_string(), Value::Null);
        hb.insert("started_at".to_string(), Value::Null);
        hb.insert("updated_at".to_string(), Value::Null);
        hb.insert("log_tail".to_string(), Value::Array(Vec::new()));

        Ctx {
            beautify_md: here.join("beautify.md"),
            github_tools_ext: here.join("github-tools.ts"),
            provision_md: here.join("provision.md"),
            ideate_md: here.join("ideate.md"),
            reflect_md: here.join("reflect.md"),
            solomon_md: here.join("solomon.md"),
            review_md: here.join("review.md"),
            plan_md: here.join("plan.md"),

            here,
            control,
            repo: repo_path,
            name: name.to_string(),
            runtime,
            heartbeat_path,
            lock_path,
            stop_path,
            log_path,
            agent_md,
            backlog,
            lessons,
            venv_py,

            provider_name,
            pi_provider,
            pi_model,
            pi_ext,
            api_key: String::new(),

            // run config defaults (run_improver module level)
            run_id,
            interval: 120,
            ship: "pr".to_string(),
            gate_cmd: String::new(),
            reasoning: String::new(),
            goal: String::new(),
            beautify: false,
            solomon: false,
            github_tools: false,
            base_branch: "main".to_string(),
            phase: "implement".to_string(),

            // pipeline toggle defaults
            review_enabled: false,
            plan_enabled: false,
            ideate_enabled: false,
            reflect_enabled: false,
            visual_review_enabled: false,
            sandbox_config: Value::Null,
            vision_model: String::new(),

            // mutable state defaults
            hb: Value::Object(hb),
            halted: false,
            dbp_self_stop: false,
            last_gate_feedback: String::new(),
            last_visual_feedback: String::new(),

            // bail counters
            dirty_base_bail_count: 0,
            unpushed_base_bail_count: 0,
            base_gate_red_bail_count: 0,

            // escalation ladder
            fail_counts: HashMap::new(),
            escalated_goals: HashSet::new(),
        }
    }

    /// run_improver._refresh_config_from_registry (~173-222): re-read THIS repo's row from repos.json
    /// at the top of each iteration so a dashboard edit to model/gate/reasoning/goal + the pipeline
    /// toggles + sandbox/vision takes effect without a stop+restart. Best-effort: a read/parse error
    /// keeps the current config (logged, never raises). SHIP / BASE_BRANCH / interval / max_iterations
    /// are launch-owned and deliberately NOT refreshed. Ends by applying the per-phase override.
    pub fn refresh_config_from_registry(&mut self) {
        // rows = json.loads((CONTROL / "repos.json").read_text()) — but the spec/source read the FILE
        // directly (not the merged load_repos), keeping a torn read transient. read_repos_json() is the
        // lenient list read of the same file; a present-but-corrupt file there yields [] which would
        // silently skip — so use the strict raw read to honor the `except (OSError, ValueError)` log path.
        let rows = match read_repos_json_raw(&self.control) {
            Ok(r) => r,
            Err(exc) => {
                // log(f"config refresh skipped: {exc}; keeping current config")
                self.log(&format!("config refresh skipped: {exc}; keeping current config"));
                return;
            }
        };
        // row = next((r for r in rows if isinstance(r, dict) and r.get("name") == NAME), None)
        let row = match rows {
            Value::Array(arr) => arr.into_iter().find(|r| {
                r.is_object() && r.get("name").and_then(Value::as_str) == Some(self.name.as_str())
            }),
            _ => None, // rows not a list -> row=None
        };
        let row = match row {
            Some(r) if r.is_object() => r,
            _ => return, // not isinstance(row, dict) -> return
        };

        // prov = PROVIDERS.get(row.get("provider") or "ollama-cloud") or PROVIDERS["ollama-cloud"]
        let provider = str_or_truthy(row.get("provider"), "ollama-cloud");
        let prov = providers_or_default(&provider);
        self.pi_provider = prov.pi_provider.to_string();
        self.pi_ext = self.here.join(prov.ext);
        // PI_MODEL = row.get("model") or prov["default_model"]
        self.pi_model = str_or_truthy(row.get("model"), prov.default_model);
        // API_KEY = row.get("api_key") or ""  (per-repo key; "" -> use the global .env key)
        self.api_key = str_or_truthy(row.get("api_key"), "");
        // GATE_CMD = (row.get("gate") or "").strip()
        self.gate_cmd = str_or_truthy(row.get("gate"), "").trim().to_string();
        // REASONING = row.get("reasoning") or "xhigh"
        self.reasoning = str_or_truthy(row.get("reasoning"), "xhigh");
        // GOAL = (row.get("goal") or "").strip()
        self.goal = str_or_truthy(row.get("goal"), "").trim().to_string();

        // pipe = row.get("pipeline") if isinstance(..., dict) else {}
        let pipe = match row.get("pipeline") {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        self.review_enabled = py_bool(pipe.get("review"));
        self.plan_enabled = py_bool(pipe.get("plan"));
        self.ideate_enabled = py_bool(pipe.get("ideate"));
        self.reflect_enabled = py_bool(pipe.get("reflect"));

        // _hb["model"] = PI_MODEL
        self.hb_set("model", json!(self.pi_model));

        // sb = row.get("sandbox"); enabled when dict AND truthy sb["enabled"]
        let sb = row.get("sandbox");
        let enabled = matches!(sb, Some(Value::Object(o)) if py_bool(o.get("enabled")));
        if enabled {
            let sb_obj = sb.cloned().unwrap_or(Value::Null);
            self.visual_review_enabled = true;
            self.vision_model = sb_obj
                .get("vision_model")
                .map(|v| str_truthy_or_empty(Some(v)))
                .unwrap_or_default()
                .trim()
                .to_string();
            self.sandbox_config = sb_obj;
        } else {
            self.visual_review_enabled = false;
            self.sandbox_config = Value::Null;
            self.vision_model = String::new();
        }

        // _apply_phase_config(row)
        self.apply_phase_config(Some(&row));

        // Re-apply the per-repo API-key override AFTER phase config (a per-phase provider override may
        // have swapped pi_provider, changing which env var the key should land in).
        self.apply_api_key();
    }

    /// run_improver._apply_phase_config (~223-257): override the active provider/model/reasoning with
    /// PHASE-specific config (`phases.<PHASE>`) + smart defaults. Precedence: explicit
    /// `phases.<phase>.X` > smart per-phase default > the repo-level config already set. Deep phases
    /// keep the repo's strong model/reasoning; light phases (beautify/e2e) drop to the cheap worker
    /// model + low reasoning. When `row` is None, re-reads repos.json (torn read -> {}).
    pub fn apply_phase_config(&mut self, row: Option<&Value>) {
        // row = passed-in, or re-read repos.json (the {} branch on any read/parse error).
        let owned_row: Value;
        let row_ref: &Value = match row {
            Some(r) => r,
            None => {
                owned_row = match read_repos_json_raw(&self.control) {
                    Ok(Value::Array(arr)) => arr
                        .into_iter()
                        .find(|r| {
                            r.is_object()
                                && r.get("name").and_then(Value::as_str)
                                    == Some(self.name.as_str())
                        })
                        .unwrap_or(Value::Object(Map::new())),
                    // rows not a list, OR read/parse error: row = {}
                    _ => Value::Object(Map::new()),
                };
                &owned_row
            }
        };

        // phases = row.get("phases") if isinstance(..., dict) else {}
        let phases = match row_ref.get("phases") {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        // pcfg = phases.get(PHASE) if isinstance(..., dict) else {}
        let pcfg = match phases.get(&self.phase) {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        // dflt = _PHASE_DEFAULTS.get(PHASE, {})
        let dflt = phase_default(&self.phase); // Option<(reasoning, cheap)>
        let dflt_cheap = dflt.map(|d| d.1).unwrap_or(false);
        let dflt_reasoning = dflt.map(|d| d.0);

        // repo_prov = row.get("provider") or "ollama-cloud"
        let repo_prov = str_or_truthy(row_ref.get("provider"), "ollama-cloud");
        // prov_name = pcfg.get("provider")  (Python: truthy check below)
        let prov_name = pcfg.get("provider").and_then(Value::as_str).unwrap_or("");

        if !prov_name.is_empty() && providers(prov_name).is_some() {
            // prov = PROVIDERS[prov_name]
            let prov = providers(prov_name).expect("checked above");
            self.pi_provider = prov.pi_provider.to_string();
            self.pi_ext = self.here.join(prov.ext);
            // PI_MODEL = pcfg.get("model") or (_CHEAP_MODEL[prov_name] if dflt.cheap) or prov.default_model
            let pcfg_model = pcfg.get("model").and_then(Value::as_str).unwrap_or("");
            self.pi_model = if !pcfg_model.is_empty() {
                pcfg_model.to_string()
            } else if dflt_cheap {
                cheap_model(prov_name)
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| prov.default_model.to_string())
            } else {
                prov.default_model.to_string()
            };
        } else {
            let pcfg_model = pcfg.get("model").and_then(Value::as_str).unwrap_or("");
            if !pcfg_model.is_empty() {
                // elif pcfg.get("model"): PI_MODEL = pcfg["model"]
                self.pi_model = pcfg_model.to_string();
            } else if dflt_cheap {
                // elif dflt.get("cheap"): PI_MODEL = _CHEAP_MODEL.get(repo_prov, PI_MODEL)
                self.pi_model = cheap_model(&repo_prov)
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| self.pi_model.clone());
            }
        }

        // REASONING = pcfg.get("reasoning") or dflt.get("reasoning") or REASONING or "xhigh"
        let pcfg_reasoning = pcfg.get("reasoning").and_then(Value::as_str).unwrap_or("");
        self.reasoning = if !pcfg_reasoning.is_empty() {
            pcfg_reasoning.to_string()
        } else if let Some(dr) = dflt_reasoning {
            dr.to_string()
        } else if !self.reasoning.is_empty() {
            self.reasoning.clone()
        } else {
            "xhigh".to_string()
        };

        // _hb["model"] = PI_MODEL ; _hb["phase"] = PHASE
        self.hb_set("model", json!(self.pi_model));
        self.hb_set("phase", json!(self.phase));
    }

    // ---- env / redaction -------------------------------------------------- #

    /// run_improver._clean_env (~500-525): the env a pi/git/gh child inherits. Strips
    /// GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME and forces UTF-8 stdio. (The Python `_clean_env`
    /// ONLY removes those four keys — the RSI_* vars and the agent-shim PATH prepend live in `run_pi`,
    /// not here — so this matches the source exactly.) Applies onto a `Command` like
    /// `crate::control::proc::apply_clean_env` (identical behavior).
    pub fn apply_clean_env(&self, cmd: &mut Command) {
        for k in ["GITHUB_TOKEN", "GH_TOKEN", "PYTHONPATH", "PYTHONHOME"] {
            cmd.env_remove(k);
        }
        cmd.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
    }

    /// run_improver._redact_keyval (~526-536): redact a NAME<sep>value credential assignment. The
    /// match's NAME already ends in a credential keyword; a non-bare credential name is redacted
    /// REGARDLESS of value shape (an all-alpha password leaks otherwise). A BARE lowercase
    /// `token`/`secret` is prose-prone, so for those redact only when the value is token-shaped
    /// (has a digit, or NAME is UPPER_SNAKE).
    fn redact_keyval(name: &str, sep: &str, value: &str) -> String {
        let ambiguous_bare = matches!(name.to_lowercase().as_str(), "token" | "secret");
        let name_is_upper = !name.is_empty()
            && name == name.to_uppercase()
            && name.chars().any(|c| c.is_alphabetic());
        let looks_secret = !ambiguous_bare
            || value.chars().any(|c| c.is_ascii_digit())
            || (name_is_upper && name.contains('_'));
        if looks_secret {
            format!("{name}{sep}[REDACTED]")
        } else {
            // m.group(0): the whole original match
            format!("{name}{sep}{value}")
        }
    }

    /// run_improver._redact (~539-562): scrub secret-shaped strings (keeping a credential's NAME +
    /// separator, replacing its value); then the literal active-provider key value (bare, any shape);
    /// then each operator-supplied `deny_terms` brand/account string (case-insensitive). Returns the
    /// input unchanged when there is nothing to redact; empty passes through.
    pub fn redact(&self, text: &str) -> String {
        if text.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for pat in secret_token_patterns() {
            out = pat.replace_all(&out, "[REDACTED]").into_owned();
        }
        let kv = secret_keyval_pattern();
        out = kv
            .replace_all(&out, |caps: &regex::Captures| {
                Self::redact_keyval(&caps[1], &caps[2], &caps[3])
            })
            .into_owned();
        // Exact-value pass: scrub the literal loaded provider key (Ollama keys aren't sk-/gh-shaped).
        for k in ["OLLAMA_API_KEY", "OPENROUTER_API_KEY"] {
            if let Ok(v) = std::env::var(k) {
                if v.len() >= 8 {
                    out = out.replace(&v, "[REDACTED]");
                }
            }
        }
        // Brand/account identity pass: scrub each operator deny-term (repos.json deny_terms),
        // case-insensitively. No-op when none configured.
        for term in self.repo_deny_terms() {
            if !term.is_empty() {
                let pat = regex::RegexBuilder::new(&regex::escape(&term))
                    .case_insensitive(true)
                    .build();
                if let Ok(p) = pat {
                    out = p.replace_all(&out, "[REDACTED]").into_owned();
                }
            }
        }
        out
    }

    /// run_improver._repo_deny_terms (~1287-1293): THIS repo's `deny_terms` list from repos.json
    /// (operator brand/account strings to scrub), read fresh. [] when absent/torn.
    fn repo_deny_terms(&self) -> Vec<String> {
        let row = self.repo_row();
        match row.get("deny_terms") {
            Some(Value::Array(arr)) => arr
                .iter()
                .map(value_to_py_str)
                .filter(|t| !t.trim().is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// run_improver._repo_row (~1260-1269): THIS repo's repos.json row read fresh (so a dashboard edit
    /// takes effect mid-loop), or {} on absent/torn/non-list/missing.
    fn repo_row(&self) -> Value {
        match read_repos_json_raw(&self.control) {
            Ok(Value::Array(arr)) => arr
                .into_iter()
                .find(|r| {
                    r.is_object()
                        && r.get("name").and_then(Value::as_str) == Some(self.name.as_str())
                })
                .unwrap_or(Value::Object(Map::new())),
            _ => Value::Object(Map::new()),
        }
    }

    /// run_improver._load_env (~568-584): load provider API keys (+ OLLAMA_BASE_URL) from
    /// Solomon/.env into the process env. Dependency-free parser; existing environment values win.
    /// Then apply the per-repo `api_key` override (if set) so this repo's iterations use its own key
    /// instead of the global one. Called once at startup; the per-repo override is re-applied each
    /// iteration by `refresh_config_from_registry` -> `apply_api_key`.
    pub fn load_env(&self) {
        let p = self.control.join(".env");
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => return, // not p.exists() / OSError -> no-op
        };
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let (k, v) = line.split_once('=').expect("contains '='");
            let k = k.trim();
            // v.strip().strip('"').strip("'") — strip outer matching quote chars (Python strip removes
            // ALL leading/trailing chars in the set; here the set is a single char each pass).
            let v = v.trim();
            let v = v.trim_matches('"');
            let v = v.trim_matches('\'');
            if ENV_KEYS.contains(&k) && std::env::var(k).map(|e| e.is_empty()).unwrap_or(true) {
                // Python: `not os.environ.get(k)` is true when unset OR empty-string.
                std::env::set_var(k, v);
            }
        }
        // Per-repo override on top of the globals.
        self.apply_api_key();
    }

    /// Apply this repo's per-repo `api_key` to the process env, overriding the global .env value for
    /// the active provider's env var. A no-op when `api_key` is empty (the global key, if any, is
    /// left in place). Idempotent; called from load_env() and refresh_config_from_registry() so a
    /// dashboard edit to the per-repo key takes effect mid-loop without a stop+restart. Each repo's
    /// improver is a separate process, so this override is isolated to that repo's iterations.
    pub fn apply_api_key(&self) {
        if self.api_key.is_empty() {
            return;
        }
        // Map the active provider to its env-var name (mirror required_key, but as a set, not a read).
        let var = if self.pi_provider == "openrouter" {
            "OPENROUTER_API_KEY"
        } else {
            "OLLAMA_API_KEY"
        };
        std::env::set_var(var, &self.api_key);
    }

    /// run_improver._required_key (~587-589): the env-var name of the API key the active provider needs.
    pub fn required_key(&self) -> String {
        if self.pi_provider == "openrouter" {
            "OPENROUTER_API_KEY".to_string()
        } else {
            "OLLAMA_API_KEY".to_string()
        }
    }

    // ---- exe discovery ---------------------------------------------------- #

    /// run_improver.pi_exe (~602-603): `_which("pi")`.
    pub fn pi_exe(&self) -> String {
        which("pi", &[])
    }

    /// run_improver.gh_exe (~606-607): `_which("gh", r"C:\Program Files\GitHub CLI\gh.exe")`.
    pub fn gh_exe(&self) -> String {
        which("gh", &[r"C:\Program Files\GitHub CLI\gh.exe"])
    }

    // ---- runtime telemetry writers --------------------------------------- #

    /// run_improver._runtime_append (~615-623): append one line to a runtime text/jsonl file.
    /// Best-effort: ensures RUNTIME exists, never errors (the loop must not break on telemetry IO).
    pub fn runtime_append(&self, path: &Path, line: &str) {
        let _ = std::fs::create_dir_all(&self.runtime);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// run_improver._runtime_atomic_write (~626-635): atomically (tmp + rename) write a runtime file
    /// the dashboard reads whole (heartbeat.json). Best-effort: ensures RUNTIME exists, never errors.
    /// tmp = path + ".tmp" (Python `path.with_suffix(path.suffix + ".tmp")` -> heartbeat.json.tmp).
    pub fn runtime_atomic_write(&self, path: &Path, text: &str) {
        let _ = std::fs::create_dir_all(&self.runtime);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if std::fs::write(&tmp, text.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// run_improver.log (~638-642): timestamp + print + append to LOG + push onto hb["log_tail"]
    /// keeping the last 20 lines.
    pub fn log(&mut self, msg: &str) {
        let line = format!("{} {}", now(), msg);
        println!("{line}");
        self.runtime_append(&self.log_path.clone(), &line);
        // _hb["log_tail"] = (_hb.get("log_tail") or [])[-19:] + [line]  (keeps the last 20)
        let mut tail: Vec<Value> = self
            .hb
            .get("log_tail")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if tail.len() > 19 {
            tail = tail.split_off(tail.len() - 19);
        }
        tail.push(json!(line));
        self.hb_set("log_tail", Value::Array(tail));
    }

    /// run_improver.heartbeat (~645-662) — CRITICAL FREEZE LOGIC, ported exactly:
    ///  1. if hb.status=="error" AND "status" not in fields -> drop "phase" from `fields` (a terminal
    ///     error heartbeat's diagnostic phase must survive a later non-status update, e.g. reflect()).
    ///  2. if fields.status is set AND != "error" -> drop a stale "reason" from hb (a fresh non-error
    ///     status starts a clean slate so a prior dirty_base_persistent reason can't outlive its cause).
    ///  3. hb.update(fields); hb["updated_at"] = now(); atomically write json.dumps(hb, indent=2).
    ///
    /// `fields` is a JSON object; pass `json!({...})`. preserve_order keeps the dashboard-visible key
    /// order: existing keys update in place, new keys append in insertion order.
    pub fn heartbeat(&mut self, fields: Value) {
        let mut fields = match fields {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        // 1. error-phase freeze
        let status_is_error = self.hb.get("status").and_then(Value::as_str) == Some("error");
        if status_is_error && !fields.contains_key("status") {
            fields.remove("phase");
        }
        // 2. fresh non-error status drops stale reason. Python `fields.get("status") not in (None,
        // "error")`: true only when status key is present AND its value is neither null nor "error".
        let drop_reason = match fields.get("status") {
            None => false,             // absent -> in (None, ...)
            Some(Value::Null) => false, // explicit null -> in (None, ...)
            Some(Value::String(s)) => s != "error",
            Some(_) => true, // any other non-null value is "not in (None, 'error')"
        };
        if drop_reason {
            if let Value::Object(hb) = &mut self.hb {
                hb.remove("reason");
            }
        }
        // 3. hb.update(fields)
        if let Value::Object(hb) = &mut self.hb {
            for (k, v) in fields {
                hb.insert(k, v);
            }
            hb.insert("updated_at".to_string(), json!(now()));
        }
        // json.dumps(_hb, indent=2) — preserve_order keeps insertion order.
        let text = serde_json::to_string_pretty(&self.hb).unwrap_or_else(|_| "{}".to_string());
        self.runtime_atomic_write(&self.heartbeat_path.clone(), &text);
    }

    /// run_improver._record_history (~665-675): append one terminal-outcome JSON line to
    /// runtime/<name>/history.jsonl (the dashboard timeline/metrics read it). rec keys (insertion
    /// order): ts, iteration, status, branch, tests, pr, summary[:500]; `extra` (when given) is merged
    /// in. Best-effort; never errors.
    pub fn record_history(
        &self,
        status: &str,
        branch: Option<&str>,
        summary: &str,
        extra: Option<&Value>,
    ) {
        let mut rec = Map::new();
        rec.insert("ts".to_string(), json!(now()));
        rec.insert(
            "iteration".to_string(),
            self.hb.get("iteration").cloned().unwrap_or(Value::Null),
        );
        rec.insert("status".to_string(), json!(status));
        rec.insert(
            "branch".to_string(),
            match branch {
                Some(b) => json!(b),
                None => Value::Null,
            },
        );
        rec.insert(
            "tests".to_string(),
            self.hb.get("tests").cloned().unwrap_or(Value::Null),
        );
        rec.insert(
            "pr".to_string(),
            self.hb.get("last_pr").cloned().unwrap_or(Value::Null),
        );
        // (summary or "")[:500] — slice by Python chars (code points), not bytes.
        rec.insert("summary".to_string(), json!(truncate_chars(summary, 500)));
        if let Some(Value::Object(e)) = extra {
            for (k, v) in e {
                rec.insert(k.clone(), v.clone());
            }
        }
        let line = serde_json::to_string(&Value::Object(rec)).unwrap_or_else(|_| "{}".to_string());
        self.runtime_append(&self.runtime.join("history.jsonl"), &line);
    }

    // ---- git / gh --------------------------------------------------------- #

    /// run_improver.git (~679-694): run `git <args>` in REPO with the scrubbed env + a BOUNDED
    /// timeout. UTF-8 decode with replacement. On timeout -> RunOut{code:124, stdout:"",
    /// stderr:"git <args> timed out after <t>s"} (a FAILED result, never an invisible hang).
    pub fn git(&self, args: &[&str], timeout: i64) -> proc::RunOut {
        let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
        argv.push("git".to_string());
        argv.extend(args.iter().map(|s| s.to_string()));
        self.run_bounded(&argv, timeout, &format!("git {}", args.join(" ")))
    }

    /// run_improver._gh (~697-709): run `gh <args>` (via gh_exe) in REPO with the same env/timeout
    /// hardening as git(). On timeout -> rc=124 + "gh <args> timed out after <t>s".
    pub fn gh(&self, args: &[&str], timeout: i64) -> proc::RunOut {
        let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
        argv.push(self.gh_exe());
        argv.extend(args.iter().map(|s| s.to_string()));
        self.run_bounded(&argv, timeout, &format!("gh {}", args.join(" ")))
    }

    /// Shared run-with-bounded-timeout body for git()/gh(): cwd=REPO, scrubbed env (proc::run already
    /// applies apply_clean_env + hidden window), UTF-8 lossy decode. Timeout maps to the Python
    /// `TimeoutExpired` -> failed CompletedProcess(rc=124) branch; a spawn failure surfaces as a
    /// non-zero RunOut with the OS error on stderr (callers treat it like a failed op).
    fn run_bounded(&self, argv: &[String], timeout: i64, timed_out_label: &str) -> proc::RunOut {
        let dur = std::time::Duration::from_secs(timeout.max(0) as u64);
        match proc::run(argv, Some(&self.repo), Some(dur)) {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => proc::RunOut {
                code: 124,
                stdout: String::new(),
                stderr: format!("{timed_out_label} timed out after {timeout}s"),
            },
            Err(e) => proc::RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    /// run_improver.has_remote (~712-713): `git remote get-url origin` returns 0.
    pub fn has_remote(&self) -> bool {
        self.git(&["remote", "get-url", "origin"], 120).code == 0
    }

    /// run_improver._branch_on_remote (~716-719): True iff `branch` is visible on origin (confirms a
    /// push landed) — `git ls-remote --heads origin <branch>` rc==0 AND "refs/heads/<branch>" in stdout.
    pub fn branch_on_remote(&self, branch: &str) -> bool {
        let r = self.git(&["ls-remote", "--heads", "origin", branch], 120);
        r.code == 0 && r.stdout.contains(&format!("refs/heads/{branch}"))
    }

    /// run_improver._github_ready (~722-729): (ok, reason) — gh authenticated AND origin reachable.
    /// DEVIATION: `_gh_ready()` (gh auth status probe) is defined elsewhere in run_improver and is
    /// out of scope for this phase; here it is approximated by `gh auth status` rc==0 so the method is
    /// self-contained. The later phase wiring can swap in the dedicated gh-auth check if it diverges.
    pub fn github_ready(&self) -> (bool, String) {
        if self.gh(&["auth", "status"], 120).code != 0 {
            return (false, "gh not authenticated (run `gh auth login`)".to_string());
        }
        if self.git(&["ls-remote", "--heads", "origin"], 120).code != 0 {
            return (false, "origin remote not reachable".to_string());
        }
        (true, String::new())
    }

    // ---- heartbeat field helper ------------------------------------------ #

    /// Set a single key on the `hb` object directly (a bare `_hb[k] = v` assignment in Python, NOT a
    /// `heartbeat(**fields)` call — so it does NOT trigger the freeze logic or rewrite updated_at).
    fn hb_set(&mut self, key: &str, value: Value) {
        if let Value::Object(hb) = &mut self.hb {
            hb.insert(key.to_string(), value);
        }
    }
}

// --------------------------------------------------------------------------- #
// free helpers
// --------------------------------------------------------------------------- #

/// Path(repo).resolve(): canonicalize when the path exists; else best-effort absolute (Python's
/// resolve() does not require existence). Strips the Windows \\?\ verbatim prefix canonicalize adds.
fn resolve_path(p: &str) -> PathBuf {
    let path = Path::new(p);
    if let Ok(c) = std::fs::canonicalize(path) {
        let s = c.to_string_lossy();
        return PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string());
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// run_improver._which (~592-599): shutil.which(name) -> first extra path that exists -> name itself
/// (last resort; the subprocess surfaces a clear error).
fn which(name: &str, extra: &[&str]) -> String {
    if let Ok(p) = which::which(name) {
        return p.to_string_lossy().into_owned();
    }
    for e in extra {
        if Path::new(e).exists() {
            return e.to_string();
        }
    }
    name.to_string()
}

/// Raw, strict read of CONTROL/repos.json mirroring run_improver's
/// `json.loads((CONTROL / "repos.json").read_text(encoding="utf-8"))`: returns the parsed Value, or
/// Err(message) on OSError/JSONDecodeError (the `except (OSError, ValueError)` branch). Unlike
/// registry::read_repos_json (lenient -> []), this keeps the present-but-corrupt error so the
/// refresh logs "config refresh skipped". A non-list parse is still Ok(value) — run_improver guards
/// the list-ness separately (`if isinstance(rows, list)`).
fn read_repos_json_raw(control: &Path) -> Result<Value, String> {
    let path = control.join("repos.json");
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
}

/// Python `(d.get(k) or default)` for a STRING result: the value when it is a truthy string, else the
/// default. A non-string truthy value would be a type bug in repos.json; we coerce to default to stay
/// safe (run_improver does `.strip()`/concatenation which would TypeError on a non-str — not a path we
/// reproduce, since the registry only ever stores strings for these keys).
fn str_or_truthy(v: Option<&Value>, default: &str) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => default.to_string(),
    }
}

/// Like str_or_truthy but the default is "" — used where Python writes `(x or "")`.
fn str_truthy_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Python `bool(x)` truthiness for a pipeline-toggle value: null/false/0/""/[]/{} -> false.
fn py_bool(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python str(x) of a deny_terms entry (run_improver does `str(t)`). Strings pass through verbatim;
/// other JSON scalars get their Python-ish text form. Objects/arrays are rare here and rendered via
/// their JSON text — never matched literally, harmless.
fn value_to_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python `s[:n]` by code points (not bytes), so a multi-byte summary truncates the same way.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bare Ctx for unit-testing the pure freeze logic without touching the filesystem.
    /// runtime points at a temp dir so any incidental write (heartbeat() persists) is harmless.
    fn test_ctx() -> Ctx {
        let mut c = Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None);
        c.runtime = std::env::temp_dir().join(format!("solomon_ctx_test_{}", run_id_hex()));
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c
    }

    // ---- now() / stamp() format ----
    #[test]
    fn now_format_is_iso_utc_z() {
        let s = now();
        // %Y-%m-%dT%H:%M:%SZ -> "2026-06-22T14:09:01Z" : 20 chars, fixed punctuation.
        assert_eq!(s.len(), 20, "got {s:?}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert!(s.ends_with('Z'));
        // every other char is a digit
        for (i, ch) in s.char_indices() {
            if [4, 7, 10, 13, 16, 19].contains(&i) {
                continue;
            }
            assert!(ch.is_ascii_digit(), "char {i} of {s:?} not a digit");
        }
    }

    #[test]
    fn stamp_format_is_compact_utc_z() {
        let s = stamp();
        // %Y%m%dT%H%M%SZ -> "20260622T140901Z" : 16 chars, a 'T' at index 8, trailing 'Z'.
        assert_eq!(s.len(), 16, "got {s:?}");
        assert_eq!(&s[8..9], "T");
        assert!(s.ends_with('Z'));
        for (i, ch) in s.char_indices() {
            if i == 8 || i == 15 {
                continue;
            }
            assert!(ch.is_ascii_digit(), "char {i} of {s:?} not a digit");
        }
    }

    // ---- heartbeat freeze logic (the 3 cases) ----

    #[test]
    fn heartbeat_error_no_status_drops_phase() {
        // CASE 1: a terminal error heartbeat's phase must survive a later non-status update.
        let mut c = test_ctx();
        c.heartbeat(json!({"status": "error", "phase": "preflight", "reason": "dirty_base_persistent"}));
        assert_eq!(c.hb["status"], json!("error"));
        assert_eq!(c.hb["phase"], json!("preflight"));
        assert_eq!(c.hb["reason"], json!("dirty_base_persistent"));
        // reflect() later: heartbeat(phase="reflect") with NO status -> phase dropped, frozen.
        c.heartbeat(json!({"phase": "reflect"}));
        assert_eq!(c.hb["phase"], json!("preflight"), "frozen error phase must NOT be clobbered");
        assert_eq!(c.hb["status"], json!("error"));
        // reason preserved (no fresh non-error status set)
        assert_eq!(c.hb["reason"], json!("dirty_base_persistent"));
    }

    #[test]
    fn heartbeat_fresh_nonerror_status_drops_reason() {
        // CASE 2: a fresh non-error status starts a clean slate -> a stale diagnostic reason is dropped.
        let mut c = test_ctx();
        c.heartbeat(json!({"status": "error", "phase": "preflight", "reason": "dirty_base_persistent"}));
        assert_eq!(c.hb["reason"], json!("dirty_base_persistent"));
        // a new iteration sets status="iterating" -> reason dropped.
        c.heartbeat(json!({"status": "iterating", "phase": "implement"}));
        assert_eq!(c.hb["status"], json!("iterating"));
        assert_eq!(c.hb["phase"], json!("implement"), "non-error status update CAN set phase");
        assert!(c.hb.get("reason").is_none(), "stale reason must be dropped on fresh non-error status");
    }

    #[test]
    fn heartbeat_normal_update_sets_fields_and_updated_at() {
        // CASE 3: a normal (non-error) update applies its fields and refreshes updated_at.
        let mut c = test_ctx();
        c.heartbeat(json!({"status": "idle", "phase": Value::Null}));
        assert_eq!(c.hb["status"], json!("idle"));
        assert_eq!(c.hb["phase"], Value::Null);
        let first = c.hb["updated_at"].as_str().unwrap().to_string();
        assert_eq!(first.len(), 20, "updated_at is an ISO Z stamp");
        // a follow-up update merges new fields, keeps prior ones.
        c.heartbeat(json!({"iteration": 3, "tests": "5 passed"}));
        assert_eq!(c.hb["iteration"], json!(3));
        assert_eq!(c.hb["tests"], json!("5 passed"));
        assert_eq!(c.hb["status"], json!("idle"), "prior status retained");
    }

    #[test]
    fn heartbeat_error_with_status_keeps_explicit_phase() {
        // An error heartbeat that DOES carry status sets phase normally (the freeze only fires when
        // status is absent from the incoming fields).
        let mut c = test_ctx();
        c.heartbeat(json!({"status": "iterating", "phase": "implement"}));
        c.heartbeat(json!({"status": "error", "phase": "reverted", "reason": "revert_failed"}));
        assert_eq!(c.hb["status"], json!("error"));
        assert_eq!(c.hb["phase"], json!("reverted"));
        assert_eq!(c.hb["reason"], json!("revert_failed"), "error+status preserves its reason");
    }

    // ---- configure defaults ----
    #[test]
    fn configure_sets_provider_and_hb_seed() {
        let c = Ctx::configure("C:/x/maki", "maki", "ollama-cloud", None);
        assert_eq!(c.provider_name, "ollama-cloud");
        assert_eq!(c.pi_provider, "maki-cloud");
        assert_eq!(c.pi_model, "glm-5.2");
        assert_eq!(c.hb["repo"], json!("maki"));
        assert_eq!(c.hb["model"], json!("glm-5.2"));
        assert_eq!(c.phase, "implement");
        assert_eq!(c.interval, 120);
        assert_eq!(c.ship, "pr");
        assert_eq!(c.run_id.len(), 32);
        // unknown provider -> ollama-cloud fallback
        let c2 = Ctx::configure("C:/x/y", "y", "bogus", Some("custom-model"));
        assert_eq!(c2.provider_name, "ollama-cloud");
        assert_eq!(c2.pi_provider, "maki-cloud");
        assert_eq!(c2.pi_model, "custom-model");
        // openrouter
        let c3 = Ctx::configure("C:/x/z", "z", "openrouter", None);
        assert_eq!(c3.pi_provider, "openrouter");
        assert_eq!(c3.pi_model, "qwen/qwen3-coder");
    }

    // ---- redact ----
    #[test]
    fn redact_scrubs_token_shapes_and_keyvals() {
        let c = test_ctx();
        let s = "key ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345 done";
        assert!(c.redact(s).contains("[REDACTED]"));
        // bare prose 'token: validation logic' (no digit, lowercase bare) is NOT redacted
        let prose = "token: validation logic";
        assert_eq!(c.redact(prose), prose);
        // an UPPER_SNAKE credential value is redacted regardless of shape
        let kv = "API_KEY=abcdefghxyz";
        assert_eq!(c.redact(kv), "API_KEY=[REDACTED]");
        // empty passes through
        assert_eq!(c.redact(""), "");
    }

    // ---- apply_api_key: per-repo key overrides the provider env var ----
    // Mutates the process env (OPENROUTER_API_KEY / OLLAMA_API_KEY); serialize via a mutex and
    // save/restore the touched vars so the suite is hermetic.
    static APIKEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        keys: Vec<&'static str>,
        saved: Vec<Option<String>>,
        _g: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvVarGuard {
        fn capture(keys: Vec<&'static str>) -> Self {
            let g = APIKEY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = keys.iter().map(|k| std::env::var(k).ok()).collect();
            for k in &keys { std::env::remove_var(k); }
            EnvVarGuard { keys, saved, _g: g }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (k, v) in self.keys.iter().zip(self.saved.iter()) {
                match v {
                    Some(s) => std::env::set_var(k, s),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn apply_api_key_openrouter_overrides_env() {
        let _g = EnvVarGuard::capture(vec!["OPENROUTER_API_KEY", "OLLAMA_API_KEY"]);
        // seed a "global" key
        std::env::set_var("OPENROUTER_API_KEY", "sk-global");
        let mut c = Ctx::configure("C:/x/repo", "repo", "openrouter", None);
        assert_eq!(c.pi_provider, "openrouter");
        // no per-repo key -> global stays
        c.api_key = String::new();
        c.apply_api_key();
        assert_eq!(std::env::var("OPENROUTER_API_KEY").unwrap(), "sk-global");
        // per-repo key overrides the global
        c.api_key = "sk-perrepo".to_string();
        c.apply_api_key();
        assert_eq!(std::env::var("OPENROUTER_API_KEY").unwrap(), "sk-perrepo");
    }

    #[test]
    fn apply_api_key_ollama_cloud_writes_ollama_var() {
        let _g = EnvVarGuard::capture(vec!["OPENROUTER_API_KEY", "OLLAMA_API_KEY"]);
        std::env::remove_var("OLLAMA_API_KEY");
        let mut c = Ctx::configure("C:/x/repo", "repo", "ollama-cloud", None);
        assert_eq!(c.pi_provider, "maki-cloud");
        c.api_key = "sk-ollama-perrepo".to_string();
        c.apply_api_key();
        assert_eq!(std::env::var("OLLAMA_API_KEY").unwrap(), "sk-ollama-perrepo");
        // openrouter var untouched
        assert!(std::env::var("OPENROUTER_API_KEY").is_err());
    }

    #[test]
    fn apply_api_key_empty_is_noop() {
        let _g = EnvVarGuard::capture(vec!["OPENROUTER_API_KEY", "OLLAMA_API_KEY"]);
        std::env::set_var("OPENROUTER_API_KEY", "sk-global");
        let mut c = Ctx::configure("C:/x/repo", "repo", "openrouter", None);
        c.api_key = String::new();
        c.apply_api_key();
        assert_eq!(std::env::var("OPENROUTER_API_KEY").unwrap(), "sk-global");
    }

    #[test]
    fn redact_scrubs_per_repo_key_value() {
        // The per-repo key, once applied to the env var, is redacted from agent text by the existing
        // exact-value pass (Ctx::redact reads OPENROUTER_API_KEY / OLLAMA_API_KEY from env).
        let _g = EnvVarGuard::capture(vec!["OPENROUTER_API_KEY", "OLLAMA_API_KEY"]);
        let mut c = Ctx::configure("C:/x/repo", "repo", "openrouter", None);
        c.api_key = "sk-or-v1-uniquerandperrepo".to_string();
        c.apply_api_key();
        let text = "here is my key sk-or-v1-uniquerandperrepo for you";
        assert!(c.redact(text).contains("[REDACTED]"), "per-repo key value must be redacted");
        assert!(!c.redact(text).contains("sk-or-v1-uniquerandperrepo"));
    }
}
