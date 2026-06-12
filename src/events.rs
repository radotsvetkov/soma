//! R1 — the journal: an append-only, hash-chained JSONL event log.
//!
//! Every significant action in soma becomes one line here. Each event embeds
//! `prev` (the SHA-256 of the previous event) and `hash` (the SHA-256 of the
//! event itself, computed over the serialized event *without* the hash field).
//! Editing or deleting any line breaks every later link, so `soma log verify`
//! can point at the first tampered line. This is the same philosophy as the
//! AGEF event chain, in greppable JSONL instead of CBOR.

use crate::json::{jint, jobj, jstr, Json};
use crate::sha256::sha256_hex;
use crate::util::*;
use std::path::{Path, PathBuf};

pub const GENESIS: &str = "genesis";

pub struct Journal {
    pub path: PathBuf,
    head_path: PathBuf,
    redact_keys: Vec<String>,
}

#[derive(Debug)]
pub struct VerifyReport {
    pub ok: bool,
    pub events: usize,
    pub head: String,
    pub first_bad: Option<(usize, String)>, // (1-based line, reason)
}

impl Journal {
    pub fn new(dir: &Path, redact_keys: Vec<String>) -> Journal {
        Journal {
            path: dir.join("events.jsonl"),
            head_path: dir.join("journal.head.json"),
            redact_keys,
        }
    }

    /// Append one event; returns the event as written (incl. its hash).
    pub fn append(&self, kind: &str, data: Json) -> R<Json> {
        let data = redact(data, &self.redact_keys);
        let (prev, count) = self.head()?;
        let ts = now_ms();
        let mut event = jobj(vec![
            ("id", jstr(new_id("ev"))),
            ("ts", jint(ts)),
            ("iso", jstr(iso8601(ts))),
            ("kind", jstr(kind)),
            ("data", data),
            ("prev", jstr(&prev)),
        ]);
        let hash = sha256_hex(event.to_string().as_bytes());
        event.set("hash", jstr(&hash));
        append_line(&self.path, &event.to_string())?;
        atomic_write(
            &self.head_path,
            jobj(vec![("hash", jstr(&hash)), ("count", jint(count + 1))])
                .to_string()
                .as_bytes(),
        )?;
        Ok(event)
    }

    /// Current chain head (hash of last event) + event count.
    /// Falls back to a full scan if the head file is missing or stale.
    fn head(&self) -> R<(String, i64)> {
        if let Ok(s) = read_to_string(&self.head_path) {
            if let Ok(j) = crate::json::parse(&s) {
                let h = j.str_of("hash");
                if !h.is_empty() {
                    return Ok((h, j.i_of("count")));
                }
            }
        }
        let mut last = GENESIS.to_string();
        let mut count = 0i64;
        for_each_line(&self.path, |line| {
            if let Ok(ev) = crate::json::parse(line) {
                let h = ev.str_of("hash");
                if !h.is_empty() {
                    last = h;
                }
            }
            count += 1;
            Ok(())
        })?;
        Ok((last, count))
    }

    /// Recompute the whole chain; detects edits, deletions, and reordering.
    pub fn verify(&self) -> R<VerifyReport> {
        let mut prev = GENESIS.to_string();
        let mut events = 0usize;
        let mut first_bad: Option<(usize, String)> = None;
        let mut line_no = 0usize;
        for_each_line(&self.path, |line| {
            line_no += 1;
            if first_bad.is_some() {
                return Ok(());
            }
            let ev = match crate::json::parse(line) {
                Ok(ev) => ev,
                Err(e) => {
                    first_bad = Some((line_no, format!("unparseable event: {e}")));
                    return Ok(());
                }
            };
            let claimed = ev.str_of("hash");
            if ev.str_of("prev") != prev {
                first_bad = Some((line_no, "broken chain: prev mismatch".into()));
                return Ok(());
            }
            // Rebuild the event without its hash field, preserving order.
            let core = match &ev {
                Json::Obj(pairs) => {
                    Json::Obj(pairs.iter().filter(|(k, _)| k != "hash").cloned().collect())
                }
                _ => {
                    first_bad = Some((line_no, "event is not an object".into()));
                    return Ok(());
                }
            };
            let recomputed = sha256_hex(core.to_string().as_bytes());
            if recomputed != claimed {
                first_bad = Some((line_no, "content hash mismatch (line edited)".into()));
                return Ok(());
            }
            prev = claimed;
            events += 1;
            Ok(())
        })?;
        Ok(VerifyReport {
            ok: first_bad.is_none(),
            events,
            head: prev,
            first_bad,
        })
    }

    pub fn tail(&self, n: usize) -> R<Vec<Json>> {
        Ok(tail_lines(&self.path, n)?
            .iter()
            .filter_map(|l| crate::json::parse(l).ok())
            .collect())
    }

    /// Stream all events through a callback (memory-bounded).
    pub fn for_each(&self, mut f: impl FnMut(&Json)) -> R<()> {
        for_each_line(&self.path, |line| {
            if let Ok(ev) = crate::json::parse(line) {
                f(&ev);
            }
            Ok(())
        })
    }
}

/// Replace values whose object key matches any redaction glob (R3) — applied
/// recursively before anything is journaled, so secrets never touch disk.
pub fn redact(v: Json, patterns: &[String]) -> Json {
    match v {
        Json::Obj(pairs) => Json::Obj(
            pairs
                .into_iter()
                .map(|(k, val)| {
                    let lower = k.to_lowercase();
                    if patterns.iter().any(|p| glob_match(p, &lower)) {
                        (k, jstr("[redacted]"))
                    } else {
                        (k, redact(val, patterns))
                    }
                })
                .collect(),
        ),
        Json::Arr(items) => Json::Arr(items.into_iter().map(|i| redact(i, patterns)).collect()),
        other => other,
    }
}

/// One-line human rendering of an event for `soma log tail`.
pub fn render_event(ev: &Json) -> String {
    let kind = ev.str_of("kind");
    let iso = ev.str_of("iso");
    let data = ev.get("data").cloned().unwrap_or(Json::Null);
    let summary = match kind.as_str() {
        "mcp.add" => format!(
            "added server '{}' → {}{}",
            data.str_of("server"),
            data.str_of("command"),
            {
                let a = data.strs_of("args");
                if a.is_empty() {
                    String::new()
                } else {
                    format!(" {}", a.join(" "))
                }
            }
        ),
        "mcp.remove" => format!("removed server '{}'", data.str_of("server")),
        "journal.anchor" => format!(
            "{} seq {} head {} via {}{}",
            data.str_of("status"),
            data.i_of("seq"),
            truncate_chars(&data.str_of("head"), 12),
            data.str_of("url"),
            {
                let r = data.str_of("reason");
                if r.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", truncate_chars(&r, 80))
                }
            }
        ),
        _ => truncate_chars(&data.to_string(), 140),
    };
    format!("{iso}  {kind:<18}  {summary}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn tmp_journal() -> (PathBuf, Journal) {
        let dir = std::env::temp_dir().join(format!("soma-j-{}", new_id("t")));
        ensure_dir(&dir).unwrap();
        let j = Journal::new(&dir, vec!["*key*".into(), "*secret*".into()]);
        (dir, j)
    }

    #[test]
    fn chain_appends_and_verifies() {
        let (dir, j) = tmp_journal();
        for i in 0..5 {
            j.append("test.event", jobj(vec![("n", jint(i))])).unwrap();
        }
        let rep = j.verify().unwrap();
        assert!(rep.ok, "{:?}", rep.first_bad);
        assert_eq!(rep.events, 5);
        assert_ne!(rep.head, GENESIS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tamper_detected() {
        let (dir, j) = tmp_journal();
        for i in 0..5 {
            j.append("test.event", jobj(vec![("n", jint(i))])).unwrap();
        }
        // Edit the payload of line 3 without recomputing hashes.
        let content = std::fs::read_to_string(&j.path).unwrap();
        let edited: Vec<String> = content
            .lines()
            .enumerate()
            .map(|(i, l)| {
                if i == 2 {
                    l.replace("\"n\":2", "\"n\":999")
                } else {
                    l.to_string()
                }
            })
            .collect();
        std::fs::write(&j.path, edited.join("\n") + "\n").unwrap();
        let rep = j.verify().unwrap();
        assert!(!rep.ok);
        assert_eq!(rep.first_bad.as_ref().unwrap().0, 3);
        assert!(rep.first_bad.unwrap().1.contains("hash mismatch"));

        // Deleting a line breaks the chain at that point too.
        let shorter: Vec<&str> = content.lines().filter(|l| !l.contains("\"n\":1")).collect();
        std::fs::write(&j.path, shorter.join("\n") + "\n").unwrap();
        let rep = j.verify().unwrap();
        assert!(!rep.ok);
        assert!(rep.first_bad.unwrap().1.contains("prev mismatch"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn secrets_redacted_before_disk() {
        let (dir, j) = tmp_journal();
        j.append(
            "model.call",
            jobj(vec![
                ("prompt", jstr("hello")),
                ("api_key", jstr("sk-ant-VERY-SECRET")),
                ("nested", jobj(vec![("client_secret", jstr("hush"))])),
            ]),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&j.path).unwrap();
        assert!(!raw.contains("VERY-SECRET"));
        assert!(!raw.contains("hush"));
        assert!(raw.contains("[redacted]"));
        // chain still valid after redaction
        assert!(j.verify().unwrap().ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn head_survives_missing_headfile() {
        let (dir, j) = tmp_journal();
        j.append("a", jobj(vec![])).unwrap();
        std::fs::remove_file(&j.head_path).unwrap();
        j.append("b", jobj(vec![])).unwrap(); // rebuilds head by scanning
        let rep = j.verify().unwrap();
        assert!(rep.ok);
        assert_eq!(rep.events, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_is_single_line() {
        let ev = parse(
            r#"{"iso":"2026-06-10T00:00:00.000Z","kind":"skill.run","data":{"x":1},"hash":"h"}"#,
        )
        .unwrap();
        let line = render_event(&ev);
        assert!(line.contains("skill.run"));
        assert!(!line.contains('\n'));
    }
}
