//! D12 — `soma export attestation`: in-toto Statement v1 over the journal head.
//!
//! The subject digest is honest by construction: the journal head IS a
//! SHA-256 over the hash-chained events, so binding it as
//! `subject.digest.sha256` attests to the exact evidence chain — it is not a
//! decorative checksum. Refuses on a broken chain, because
//! `predicate.chain.verified:true` must never be emittable falsely.
//!
//! soma does NOT sign the statement (zero-dependency stance): the output is
//! the input for `cosign` / `gh attestation` in CI — see docs/CI.md and
//! ci/github-action/. The predicate reuses aiact's single-pass journal
//! collector (D11) rather than re-walking the chain.

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::project::{Ctx, SOMA_VERSION};
use crate::util::*;
use std::path::PathBuf;

pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://github.com/radotsvetkov/soma/evidence/v1";

/// Generate the attestation file. Returns the written path.
///
/// `subject` overrides the subject name (default: project name); `out` is the
/// output FILE path (otlp/eu-ai-act precedent: a directory places the
/// default-named file inside it).
pub fn export_attestation(c: &Ctx, subject: Option<&str>, out: Option<&str>) -> R<PathBuf> {
    // Verify the chain BEFORE writing anything (export precedent). This is
    // the load-bearing gate: the statement asserts chain.verified:true, so a
    // broken chain must refuse loudly, never emit.
    let report = c.journal().verify()?;
    if !report.ok {
        let (line, why) = report.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!(
            "journal failed verification at line {line}: {why} — refusing to export"
        ));
    }
    if report.events == 0 {
        return Err("journal is empty — nothing to attest".into());
    }

    let now = now_ms();
    let stamp = {
        let p = utc_parts(now);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            p.year, p.month, p.day, p.hour, p.minute, p.second
        )
    };
    let project = c.name();
    let file_name = format!("{project}-attestation-{stamp}.json");
    let out_path = match out {
        Some(o) => {
            let p = expand_home(o);
            if p.is_dir() {
                p.join(&file_name)
            } else {
                p
            }
        }
        None => {
            let exports = c.root.join("exports");
            ensure_dir(&exports)?;
            exports.join(&file_name)
        }
    };

    // Gate a user-supplied --out by writable_paths and journal the decision
    // (R3) — same boundary the bundle export enforces, BEFORE any write.
    if out.is_some() {
        crate::export::gate_out_path(c, &out_path)?;
    }

    let col = crate::aiact::collect(c)?;
    // kinds histogram: {<kind>: count, …} — BTreeMap order, deterministic.
    let kinds: Vec<(String, Json)> = col
        .kinds
        .iter()
        .map(|(kind, st)| (kind.clone(), jint(st.count)))
        .collect();

    let statement = jobj(vec![
        ("_type", jstr(STATEMENT_TYPE)),
        (
            "subject",
            jarr(vec![jobj(vec![
                ("name", jstr(subject.unwrap_or(project.as_str()))),
                ("digest", jobj(vec![("sha256", jstr(&report.head))])),
            ])]),
        ),
        ("predicateType", jstr(PREDICATE_TYPE)),
        (
            "predicate",
            jobj(vec![
                ("soma_version", jstr(SOMA_VERSION)),
                ("project", jstr(&project)),
                ("generated", jstr(&iso8601(now))),
                ("event_count", jint(report.events as i64)),
                ("first_event", jstr(&col.first_iso)),
                ("last_event", jstr(&col.last_iso)),
                ("kinds", Json::Obj(kinds)),
                (
                    "policy",
                    jobj(vec![
                        ("autonomy", jstr(&c.policy.autonomy)),
                        (
                            "deny_commands_count",
                            jint(c.policy.deny_commands.len() as i64),
                        ),
                        (
                            "allow_commands_count",
                            jint(c.policy.allow_commands.len() as i64),
                        ),
                        (
                            "network",
                            jobj(vec![
                                ("enabled", jbool(c.policy.allow_network)),
                                ("hosts_count", jint(c.policy.allow_hosts.len() as i64)),
                            ]),
                        ),
                    ]),
                ),
                // Granted journal.anchor data objects pass through verbatim
                // (D10 implementer's note) — failed attempts never appear.
                ("anchors", jarr(col.anchors.clone())),
                (
                    "chain",
                    jobj(vec![
                        ("seq", jint(report.events as i64)),
                        ("head", jstr(&report.head)),
                        // True by construction: the verify() gate above
                        // refused already if it weren't.
                        ("verified", jbool(true)),
                    ]),
                ),
            ]),
        ),
    ]);

    atomic_write(&out_path, statement.pretty().as_bytes())?;

    // Journal as export.bundle with format:"attestation" (otlp/eu-ai-act
    // precedent — the cockpit kind map is frozen for v6, no new kind).
    c.log(
        "export.bundle",
        jobj(vec![
            ("dir", jstr(out_path.to_string_lossy().as_ref())),
            ("events", jint(report.events as i64)),
            ("head", jstr(&report.head)),
            ("format", jstr("attestation")),
        ]),
    )?;

    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::jobj as o;
    use crate::project::testutil::temp_ctx;

    /// Generate with the LIVE head captured first; returns (statement, path, head).
    fn gen(c: &Ctx, subject: Option<&str>) -> (Json, PathBuf, String) {
        let head = c.journal().verify().unwrap().head;
        let path = export_attestation(c, subject, None).unwrap();
        let j = crate::json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        (j, path, head)
    }

    #[test]
    fn statement_shape_exact_keys() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (j, path, _) = gen(&c, None);
        // Top level: the four in-toto Statement v1 members, exact values.
        assert_eq!(j.str_of("_type"), STATEMENT_TYPE);
        assert_eq!(j.str_of("predicateType"), PREDICATE_TYPE);
        let subject = j.get("subject").unwrap().arr().unwrap().clone();
        assert_eq!(subject.len(), 1);
        assert_eq!(subject[0].str_of("name"), "testproj");
        assert_eq!(subject[0].get("digest").unwrap().str_of("sha256").len(), 64);
        // Predicate: every documented key present (JSON-API.md stability).
        let p = j.get("predicate").unwrap();
        for key in [
            "soma_version",
            "project",
            "generated",
            "event_count",
            "first_event",
            "last_event",
            "kinds",
            "policy",
            "anchors",
            "chain",
        ] {
            assert!(p.get(key).is_some(), "predicate missing key {key}");
        }
        assert_eq!(p.str_of("soma_version"), SOMA_VERSION);
        assert_eq!(p.str_of("project"), "testproj");
        assert!(p.i_of("event_count") >= 2);
        assert_eq!(p.get("kinds").unwrap().i_of("test.event"), 1);
        let pol = p.get("policy").unwrap();
        assert!(!pol.str_of("autonomy").is_empty());
        assert!(pol.get("deny_commands_count").is_some());
        assert!(pol.get("allow_commands_count").is_some());
        let net = pol.get("network").unwrap();
        assert!(net.get("enabled").is_some());
        assert!(net.get("hosts_count").is_some());
        let chain = p.get("chain").unwrap();
        assert!(chain.b_of("verified"));
        assert_eq!(chain.i_of("seq"), p.i_of("event_count"));
        // Default filename shape.
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(fname.starts_with("testproj-attestation-") && fname.ends_with(".json"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn subject_digest_equals_live_head() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (j, _, head) = gen(&c, None);
        let digest = j.get("subject").unwrap().arr().unwrap()[0]
            .get("digest")
            .unwrap()
            .str_of("sha256");
        assert_eq!(digest, head, "subject digest must be the live journal head");
        // chain.head agrees with the subject digest — one head, stated twice.
        let chain = j.get("predicate").unwrap().get("chain").unwrap().clone();
        assert_eq!(chain.str_of("head"), head);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn subject_flag_overrides_name_not_digest() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (j, _, head) = gen(&c, Some("my-release-artifact"));
        let s = j.get("subject").unwrap().arr().unwrap()[0].clone();
        assert_eq!(s.str_of("name"), "my-release-artifact");
        assert_eq!(s.get("digest").unwrap().str_of("sha256"), head);
        // predicate.project stays the real project name — the override names
        // the subject, it does not rewrite provenance.
        assert_eq!(j.get("predicate").unwrap().str_of("project"), "testproj");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn refuses_broken_chain() {
        let (base, c) = temp_ctx();
        c.log("a", o(vec![])).unwrap();
        c.log("b", o(vec![])).unwrap();
        let jp = c.dir.join("events.jsonl");
        let content = std::fs::read_to_string(&jp)
            .unwrap()
            .replace("\"a\"", "\"x\"");
        std::fs::write(&jp, content).unwrap();
        let err = export_attestation(&c, None, None).unwrap_err();
        assert!(err.contains("refusing to export"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn journaled_as_export_bundle_format_attestation() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (j, path, head) = gen(&c, None);
        let tail = c.journal().tail(2).unwrap();
        let ev = tail
            .iter()
            .find(|e| e.str_of("kind") == "export.bundle")
            .expect("export.bundle event");
        let data = ev.get("data").unwrap();
        assert_eq!(data.str_of("format"), "attestation");
        assert_eq!(data.str_of("dir"), path.to_string_lossy());
        // The receipt records the attested head — the cross-link a reviewer
        // checks between attestation and bundle (docs/CI.md).
        assert_eq!(data.str_of("head"), head);
        assert_eq!(
            data.i_of("events"),
            j.get("predicate").unwrap().i_of("event_count")
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anchors_granted_pass_through_verbatim_failed_excluded() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let rep = c.journal().verify().unwrap();
        c.log(
            "journal.anchor",
            o(vec![
                ("seq", jint(rep.events as i64)),
                ("head", jstr(&rep.head)),
                ("url", jstr("https://freetsa.org/tsr")),
                ("tsq_file", jstr("anchor-2-test.tsq")),
                ("tsr_file", jstr("anchor-2-test.tsr")),
                ("tsr_sha256", jstr("deadbeef")),
                ("status", jstr("granted")),
            ]),
        )
        .unwrap();
        c.log(
            "journal.anchor",
            o(vec![
                ("seq", jint(99)),
                ("status", jstr("failed")),
                ("reason", jstr("policy refused")),
            ]),
        )
        .unwrap();
        let (j, _, _) = gen(&c, None);
        let anchors = j
            .get("predicate")
            .unwrap()
            .get("anchors")
            .unwrap()
            .arr()
            .unwrap()
            .clone();
        assert_eq!(anchors.len(), 1, "granted only — failed attempts excluded");
        let a = &anchors[0];
        // Verbatim pass-through: every D10 field as journaled.
        assert_eq!(a.str_of("status"), "granted");
        assert_eq!(a.str_of("head"), rep.head);
        assert_eq!(a.str_of("url"), "https://freetsa.org/tsr");
        assert_eq!(a.str_of("tsq_file"), "anchor-2-test.tsq");
        assert_eq!(a.str_of("tsr_file"), "anchor-2-test.tsr");
        assert_eq!(a.str_of("tsr_sha256"), "deadbeef");
        assert_eq!(a.i_of("seq"), rep.events as i64);
        std::fs::remove_dir_all(&base).ok();
    }
}
