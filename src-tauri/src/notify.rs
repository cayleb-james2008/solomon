//! Operator notification channel (Solomon v2, Phase A) — probes reach the OPERATOR, not a log file.
//!
//! The June outage post-mortem: Sover posted nothing for 5 days and Asmodeus's equity flatlined
//! while the watchdog wrote "all healthy" to `runtime/_watchdog.out.log` — a file nobody reads.
//! This module is the telling half of the fix (the ops probe plane is the reading half): every
//! ops-plane incident TRANSITION (red / recovered) and every CEO-rhythm report is pushed to the
//! operator via ntfy.sh (phone, free, no account) plus a best-effort Windows toast.
//!
//! Channel config (all optional, all in the gitignored `<HERE>/.env` — the repo is public, the
//! topic is a capability URL and must never enter git):
//!   - `NTFY_TOPIC=<topic>`  — enables ntfy push to https://ntfy.sh/<topic>
//!   - `SOLOMON_NOTIFY_OFF=1` (env line or process env) — kill-switch, silences everything
//!
//! Delivery is BEST-EFFORT and bounded: curl.exe (ships with Windows 10+) with a 10 s timeout for
//! ntfy, PowerShell WinRT toast with a 10 s timeout. A delivery failure never fails a sweep and
//! never panics; every attempt is appended to `runtime/_notify.jsonl` (append-only, OSError ->
//! pass — the same contract as the watchdog's monitor log) so "was the operator actually told?"
//! is answerable from disk.
#![allow(dead_code)]

use crate::control::{paths, proc};
use chrono::Utc;
use serde_json::{json, Value};
use std::time::Duration;

/// A single operator notice. `priority` is the ntfy priority wire word:
/// "urgent" (red incidents), "default" (recoveries, reports), "low" (morning plans).
#[derive(Debug, Clone)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub priority: &'static str,
    /// ntfy tags header (emoji shortcodes), e.g. "rotating_light" / "white_check_mark".
    pub tags: &'static str,
}

impl Notice {
    pub fn red(title: String, body: String) -> Notice {
        Notice { title, body, priority: "urgent", tags: "rotating_light" }
    }
    pub fn recovered(title: String, body: String) -> Notice {
        Notice { title, body, priority: "default", tags: "white_check_mark" }
    }
    pub fn report(title: String, body: String) -> Notice {
        Notice { title, body, priority: "default", tags: "clipboard" }
    }
    pub fn plan(title: String, body: String) -> Notice {
        Notice { title, body, priority: "low", tags: "sunrise" }
    }
}

/// Read one KEY=value from `<HERE>/.env` with control/keys.rs `keys_status` semantics: last
/// duplicate wins, value is whitespace-stripped then quote-stripped ('"' pass before "'" pass).
/// None when the file is unreadable or the key is missing/empty.
pub fn env_value(key: &str) -> Option<String> {
    let content = std::fs::read_to_string(paths::env_file()).ok()?;
    let mut found: Option<String> = None;
    for line in content.lines() {
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k.trim() == key {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            found = Some(v.to_string()); // last duplicate wins
        }
    }
    found.filter(|v| !v.is_empty())
}

/// The global kill-switch: process env OR .env line `SOLOMON_NOTIFY_OFF=1`.
fn notify_off() -> bool {
    if std::env::var("SOLOMON_NOTIFY_OFF").map(|v| v == "1").unwrap_or(false) {
        return true;
    }
    env_value("SOLOMON_NOTIFY_OFF").as_deref() == Some("1")
}

/// Test-only serialization for the process-global `SOLOMON_NOTIFY_OFF` kill-switch + the shared
/// runtime/_notify.jsonl log. Any test (here OR in watchdog) that flips the env var must hold this,
/// so parallel tests in the same binary don't race the env / log. Mirrors control::keys ENV_LOCK.
#[cfg(test)]
pub(crate) static NOTIFY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One header/body line must stay a single line for curl `-H`; collapse breaks to " / ".
fn one_line(s: &str) -> String {
    s.replace("\r\n", " / ").replace(['\r', '\n'], " / ")
}

/// The curl argv for an ntfy publish (pure — unit-tested). Body goes via `-d`, headers carry
/// title/priority/tags. `-s` keeps the hidden window quiet; `-m 10` bounds the sweep cost.
pub fn curl_args(topic: &str, n: &Notice) -> Vec<String> {
    vec![
        "curl".into(),
        "-s".into(),
        "-m".into(),
        "10".into(),
        "-H".into(),
        format!("Title: {}", one_line(&n.title)),
        "-H".into(),
        format!("Priority: {}", n.priority),
        "-H".into(),
        format!("Tags: {}", n.tags),
        "-d".into(),
        n.body.clone(),
        format!("https://ntfy.sh/{topic}"),
    ]
}

/// Minimal XML text escaping for the toast payload (pure — unit-tested).
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The PowerShell -Command string for a WinRT toast (pure — unit-tested). Single-quoted PS string
/// literals with '' escaping; the AppId is PowerShell's own (registered on every Windows box) so
/// the toast shows without any app registration of our own.
pub fn toast_command(title: &str, body: &str) -> String {
    let xml = format!(
        "<toast><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual></toast>",
        xml_escape(&one_line(title)),
        xml_escape(&one_line(body))
    );
    let ps_xml = xml.replace('\'', "''");
    format!(
        "$null = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]; \
         $null = [Windows.UI.Notifications.ToastNotification, Windows.UI.Notifications, ContentType = WindowsRuntime]; \
         $null = [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom, ContentType = WindowsRuntime]; \
         $x = New-Object Windows.Data.Xml.Dom.XmlDocument; \
         $x.LoadXml('{ps_xml}'); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show((New-Object Windows.UI.Notifications.ToastNotification($x)))"
    )
}

/// Send one notice over every configured channel. Never panics, never blocks past ~20 s total.
/// Returns the per-channel delivery record (also appended to runtime/_notify.jsonl).
pub fn send(n: &Notice) -> Value {
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if notify_off() {
        // Kill-switch: deliver nothing AND log nothing. The kill-switch record was pure theater —
        // ~78% of runtime/_notify.jsonl was `{"title":"t",...,"ntfy":"off","toast":"off"}` from the
        // send() unit test writing to the live log every `cargo test`, drowning the real red/recovered
        // pages the GUI's notify_tail surfaces. A suppressed send is not a delivery attempt, so it has
        // no place in the append-only DELIVERY log. Callers still get the honest off/off record.
        return json!({"ts": ts, "title": n.title, "priority": n.priority,
                      "ntfy": "off", "toast": "off"});
    }
    let record = {
        // ntfy: only when a topic is configured; a missing topic is an honest "skipped", not an error.
        let ntfy = match env_value("NTFY_TOPIC") {
            Some(topic) => {
                let args = curl_args(&topic, n);
                match proc::run(&args, None, Some(Duration::from_secs(15))) {
                    Ok(r) if r.ok() => "sent".to_string(),
                    Ok(r) => format!("error: curl exit {} {}", r.code, one_line(r.stderr.trim())),
                    Err(e) => format!("error: {e}"),
                }
            }
            None => "skipped (no NTFY_TOPIC in .env)".to_string(),
        };
        let cmd = toast_command(&n.title, &n.body);
        let toast_args = ["powershell", "-NoProfile", "-NonInteractive", "-Command", cmd.as_str()];
        let toast = match proc::run(&toast_args, None, Some(Duration::from_secs(10))) {
            Ok(r) if r.ok() => "shown".to_string(),
            Ok(r) => format!("error: powershell exit {}", r.code),
            Err(e) => format!("error: {e}"),
        };
        json!({"ts": ts, "title": n.title, "priority": n.priority, "ntfy": ntfy, "toast": toast})
    };
    append_log(&record);
    record
}

/// HERE/runtime/_notify.jsonl — append-only delivery log (OSError -> pass).
fn append_log(record: &Value) {
    let _ = (|| -> std::io::Result<()> {
        let path = paths::here().join("runtime").join("_notify.jsonl");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", serde_json::to_string(record).unwrap_or_default())?;
        Ok(())
    })();
}

/// Map one ops incident record (see ops::outcomes::evolve) to a Notice (pure — unit-tested).
/// Unknown/malformed records map to None and are silently skipped, never a panic.
pub fn incident_notice(record: &Value) -> Option<Notice> {
    let event = record.get("event").and_then(Value::as_str)?;
    let probe_id = record.get("probe_id").and_then(Value::as_str).unwrap_or("?");
    let detail = record.get("detail").and_then(Value::as_str).unwrap_or("");
    match event {
        "red" => Some(Notice::red(
            format!("Solomon: {probe_id} RED"),
            format!("{detail}\nsince {}", record.get("first_red_ts").and_then(Value::as_str).unwrap_or("now")),
        )),
        "recovered" => Some(Notice::recovered(
            format!("Solomon: {probe_id} recovered"),
            detail.to_string(),
        )),
        _ => None,
    }
}

/// Push every incident transition from this sweep to the operator. The caller (ops sweep) already
/// dedupes: a persisting red appends nothing, so this can never spam a standing outage.
pub fn notify_incidents(records: &[Value]) {
    for r in records {
        if let Some(n) = incident_notice(r) {
            let _ = send(&n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- curl argv (pure) --------
    #[test]
    fn curl_args_shape_and_title_flattening() {
        let n = Notice::red("t1\nline2".into(), "body".into());
        let args = curl_args("mytopic", &n);
        assert_eq!(args[0], "curl");
        assert!(args.contains(&"Title: t1 / line2".to_string()));
        assert!(args.contains(&"Priority: urgent".to_string()));
        assert!(args.contains(&"Tags: rotating_light".to_string()));
        assert_eq!(args.last().unwrap(), "https://ntfy.sh/mytopic");
        // body rides -d unflattened (ntfy bodies may be multi-line)
        let d_idx = args.iter().position(|a| a == "-d").unwrap();
        assert_eq!(args[d_idx + 1], "body");
    }

    // -------- toast command (pure) --------
    #[test]
    fn toast_command_escapes_xml_and_ps_quotes() {
        let cmd = toast_command("a<b>&\"c\"", "it's 'quoted'");
        // XML metacharacters never reach LoadXml raw.
        assert!(cmd.contains("a&lt;b&gt;&amp;&quot;c&quot;"));
        // The XML-escaped apostrophe (&apos;) is itself PS-single-quote safe; no raw single quotes
        // may survive inside the PS '...' literal except as doubled ''.
        assert!(cmd.contains("it&apos;s &apos;quoted&apos;"));
        assert!(cmd.contains("LoadXml"));
        assert!(cmd.contains("ToastText02"));
    }

    #[test]
    fn xml_escape_vectors() {
        assert_eq!(xml_escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
        assert_eq!(xml_escape("plain"), "plain");
    }

    // -------- incident mapping (pure) --------
    #[test]
    fn incident_notice_maps_red_and_recovered() {
        let red = serde_json::json!({
            "event": "red", "probe_id": "asmodeus/fills_recency",
            "detail": "age 90000s > 86400s", "first_red_ts": "2026-07-01T00:00:00Z"
        });
        let n = incident_notice(&red).unwrap();
        assert_eq!(n.title, "Solomon: asmodeus/fills_recency RED");
        assert_eq!(n.priority, "urgent");
        assert!(n.body.contains("age 90000s"));
        assert!(n.body.contains("since 2026-07-01T00:00:00Z"));

        let rec = serde_json::json!({
            "event": "recovered", "probe_id": "sover/publish_recency", "detail": "age 120s"
        });
        let n = incident_notice(&rec).unwrap();
        assert_eq!(n.title, "Solomon: sover/publish_recency recovered");
        assert_eq!(n.priority, "default");

        // unknown/malformed records are skipped, never a panic
        assert!(incident_notice(&serde_json::json!({"event": "purple"})).is_none());
        assert!(incident_notice(&serde_json::json!({})).is_none());
    }

    // -------- kill-switch: send() must be a silent no-op delivery-wise AND log nothing --------
    // Pre-fix, this test appended `{"title":"t",...,"ntfy":"off","toast":"off"}` to the LIVE
    // runtime/_notify.jsonl every `cargo test` run — ~78% of the operator's notify log was this
    // one probe's theater. Now a suppressed send delivers the honest off/off record to the caller
    // but writes NOTHING to the delivery log.
    #[test]
    fn send_honors_kill_switch_and_does_not_pollute_the_log() {
        let _env = super::NOTIFY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let path = paths::here().join("runtime").join("_notify.jsonl");
        let lines_before = std::fs::read_to_string(&path).map(|s| s.lines().count()).unwrap_or(0);

        let rec = send(&Notice::report("t".into(), "b".into()));
        assert_eq!(rec["ntfy"], "off");
        assert_eq!(rec["toast"], "off");

        let lines_after = std::fs::read_to_string(&path).map(|s| s.lines().count()).unwrap_or(0);
        assert_eq!(
            lines_after, lines_before,
            "a kill-switched send must not append to the live _notify.jsonl"
        );
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- Part 2: an OUTCOME-probe RED transition pages the operator LOUDLY (urgent) --------
    // The outcome probes (publish_recency/produce_recency/process/fills_recency/lane_freshness) are
    // the real-outage signal. A green/yellow->red transition record (as emitted by
    // ops::outcomes::evolve) must map to an URGENT ntfy page with the rotating_light tag, so the
    // 37 h sover outage / asmodeus trader death page loudly instead of dying in a log file. The
    // upstream evolve() log-once dedupe means a persisting red maps once — no re-page every sweep.
    #[test]
    fn outcome_red_transition_pages_urgent() {
        for probe in [
            "sover/publish_recency",
            "sover/produce_recency",
            "asmodeus/fills_recency",
            "asmodeus/process",
            "daedalus/lane_freshness",
        ] {
            let red = serde_json::json!({
                "event": "red", "probe_id": probe,
                "detail": "no output in 133200s > 86400s",
                "first_red_ts": "2026-07-01T00:00:00Z"
            });
            let n = incident_notice(&red).expect("a red incident must map to a notice");
            assert_eq!(n.priority, "urgent", "{probe} RED must page at urgent priority");
            assert_eq!(n.tags, "rotating_light", "{probe} RED must carry the loud tag");
            assert!(n.title.contains(probe), "{probe} title: {}", n.title);
        }
    }
}
