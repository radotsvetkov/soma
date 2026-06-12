//! R2 — exportable history: a portable, offline-verifiable evidence bundle.
//!
//! `soma export` writes a directory (and .tar.gz) containing the full journal,
//! state snapshots, and a manifest with the SHA-256 of every file plus the
//! journal head hash. Anyone can verify it with `shasum -a 256` and a JSON
//! reader — no soma required. For signed, CBOR-canonical AGEF bundles, feed
//! `events.jsonl` to Akmon; this format is deliberately the greppable cousin.
//!
//! `soma export otlp` converts the journal into an OTLP/JSON
//! `ExportTraceServiceRequest` document that `akmon otel import` accepts
//! directly, enabling the full soma→akmon→agef-verify pipeline.

use crate::json::{jarr, jint, jobj, jstr, Json};
use crate::project::{Ctx, SOMA_VERSION};
use crate::sha256::sha256_hex;
use crate::util::*;
use std::path::{Path, PathBuf};

/// Files copied into the bundle when present (journal first, then state).
const STATE_FILES: [&str; 9] = [
    "events.jsonl",
    "config.json",
    "policy.json",
    "metrics.json",
    "issues.jsonl",
    "proposals.jsonl",
    "knowledge.jsonl",
    "goals.jsonl",
    "crons.json",
];

/// Gate a user-supplied `--out` path against `writable_paths` (R3) and journal
/// the decision, exactly as the bundle export does. Every export sub-format
/// (bundle/otlp/eu-ai-act/attestation) routes its `--out` through this BEFORE
/// writing, so the documented invariant — "a user-supplied --out is gated by
/// writable_paths and the decision journaled" — holds uniformly. `target` is
/// the resolved write path (home already expanded). Returns it on allow, or an
/// Err on deny (the refusal is already on the chain).
pub fn gate_out_path(c: &Ctx, target: &Path) -> R<PathBuf> {
    let target_str = target.to_string_lossy().to_string();
    let dec = c
        .policy
        .check_path_write(&target_str, c.root.to_string_lossy().as_ref());
    c.log(
        "policy.decision",
        dec.to_json(&format!("export.write:{target_str}")),
    )?;
    if !dec.allowed() {
        return Err(format!("export path blocked by policy ({})", dec.rule()));
    }
    Ok(target.to_path_buf())
}

pub fn export(c: &Ctx, out: Option<&str>) -> R<PathBuf> {
    let stamp = {
        let p = utc_parts(now_ms());
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            p.year, p.month, p.day, p.hour, p.minute, p.second
        )
    };
    let bundle_name = format!("{}-{stamp}.soma-export", c.name());
    let bundle_dir = match out {
        Some(o) => expand_home(o).join(&bundle_name),
        None => c.root.join("exports").join(&bundle_name),
    };

    // A user-supplied --out is the one path soma writes on the operator's
    // behalf, so it is gated by the writable_paths boundary (R3) and the
    // decision journaled. The default exports/ dir is always inside-project.
    if out.is_some() {
        gate_out_path(c, &bundle_dir)?;
    }

    // Verify the chain BEFORE creating anything — an export of a broken
    // journal should fail loudly, not leave a plausible-looking artifact.
    let report = c.journal().verify()?;
    if !report.ok {
        let (line, why) = report.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!(
            "journal failed verification at line {line}: {why} — refusing to export"
        ));
    }
    ensure_dir(&bundle_dir)?;

    // First/last journal event timestamps for the manifest time range (R2),
    // plus granted anchors for the VERIFY.md ANCHORS section (D10).
    let mut first_iso = String::new();
    let mut last_iso = String::new();
    let mut anchors_meta: Vec<Json> = Vec::new();
    c.journal().for_each(|ev| {
        if first_iso.is_empty() {
            first_iso = ev.str_of("iso");
        }
        last_iso = ev.str_of("iso");
        if ev.str_of("kind") == "journal.anchor" {
            if let Some(d) = ev.get("data") {
                if d.str_of("status") == "granted" {
                    anchors_meta.push(d.clone());
                }
            }
        }
    })?;

    let mut files: Vec<(String, Json)> = Vec::new();
    for name in STATE_FILES {
        let src = c.dir.join(name);
        if !src.is_file() {
            continue;
        }
        let bytes = ctx(std::fs::read(&src), &format!("read {}", src.display()))?;
        atomic_write(&bundle_dir.join(name), &bytes)?;
        files.push((
            name.to_string(),
            jobj(vec![
                ("sha256", jstr(&sha256_hex(&bytes))),
                ("bytes", jint(bytes.len() as i64)),
            ]),
        ));
    }

    // Bundle .soma/anchors/ (RFC 3161 .tsq/.tsr pairs, D10) — the .tsq
    // enables the strongest third-party check (openssl ts -verify -queryfile).
    let anchors_src = c.dir.join("anchors");
    if anchors_src.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&anchors_src)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect())
            .unwrap_or_default();
        paths.sort();
        if !paths.is_empty() {
            ensure_dir(&bundle_dir.join("anchors"))?;
        }
        for p in paths {
            let fname = p
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            if fname.is_empty() {
                continue;
            }
            let bytes = ctx(std::fs::read(&p), &format!("read {}", p.display()))?;
            atomic_write(&bundle_dir.join("anchors").join(&fname), &bytes)?;
            files.push((
                format!("anchors/{fname}"),
                jobj(vec![
                    ("sha256", jstr(&sha256_hex(&bytes))),
                    ("bytes", jint(bytes.len() as i64)),
                ]),
            ));
        }
    }

    // Snapshot the skill registry (project + global) as one JSON array.
    let mut skills: Vec<Json> = Vec::new();
    for dir in [c.skills_dir(), c.global_skills_dir()] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut paths: Vec<PathBuf> =
                entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(s) = read_to_string(&p) {
                        if let Ok(j) = crate::json::parse(&s) {
                            skills.push(j);
                        }
                    }
                }
            }
        }
    }
    let skills_bytes = Json::Arr(skills).pretty().into_bytes();
    atomic_write(&bundle_dir.join("skills.json"), &skills_bytes)?;
    files.push((
        "skills.json".to_string(),
        jobj(vec![
            ("sha256", jstr(&sha256_hex(&skills_bytes))),
            ("bytes", jint(skills_bytes.len() as i64)),
        ]),
    ));

    let manifest = jobj(vec![
        (
            "producer",
            jobj(vec![("name", jstr("soma")), ("version", jstr(SOMA_VERSION))]),
        ),
        ("project", jstr(&c.name())),
        ("created_at", jstr(&iso8601(now_ms()))),
        ("event_count", jint(report.events as i64)),
        (
            "time_range",
            jobj(vec![("from", jstr(&first_iso)), ("to", jstr(&last_iso))]),
        ),
        ("journal_head", jstr(&report.head)),
        ("hash_algorithm", jstr("sha256")),
        ("files", Json::Obj(files)),
    ]);
    atomic_write(&bundle_dir.join("manifest.json"), manifest.pretty().as_bytes())?;
    atomic_write(
        &bundle_dir.join("VERIFY.md"),
        verify_instructions(&bundle_name, &anchors_meta).as_bytes(),
    )?;

    // tar.gz next to the directory, via the system tar (journaled below).
    let tar_path = bundle_dir.with_extension("soma-export.tar.gz");
    let parent = bundle_dir.parent().unwrap_or(Path::new("."));
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(parent)
        .arg(&bundle_name)
        .status();
    let tarred = matches!(status, Ok(s) if s.success());

    c.log(
        "export.bundle",
        jobj(vec![
            ("dir", jstr(bundle_dir.to_string_lossy().as_ref())),
            ("events", jint(report.events as i64)),
            ("head", jstr(&report.head)),
            ("tarball", crate::json::jbool(tarred)),
        ]),
    )?;
    Ok(bundle_dir)
}

/// Recompute every file hash in a bundle against its manifest, and re-verify
/// the journal chain inside. Works on any machine with soma; the VERIFY.md in
/// the bundle documents the no-soma path.
pub fn verify_bundle(dir: &Path) -> R<String> {
    let manifest = crate::json::parse(&read_to_string(&dir.join("manifest.json"))?)
        .map_err(|e| format!("manifest.json: {e}"))?;
    let files = manifest
        .get("files")
        .and_then(|f| f.obj().cloned())
        .ok_or("manifest has no files map")?;
    let mut checked = 0usize;
    for (name, meta) in &files {
        let bytes = ctx(
            std::fs::read(dir.join(name)),
            &format!("read {name} from bundle"),
        )?;
        let want = meta.str_of("sha256");
        let got = sha256_hex(&bytes);
        if want != got {
            return Err(format!("{name}: sha256 mismatch (manifest {want}, actual {got})"));
        }
        checked += 1;
    }
    // Chain check on the bundled journal.
    let j = crate::events::Journal::new(dir, vec![]);
    let rep = j.verify()?;
    if !rep.ok {
        let (line, why) = rep.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!("bundled journal broken at line {line}: {why}"));
    }
    let head_want = manifest.str_of("journal_head");
    if rep.head != head_want {
        return Err(format!(
            "journal head mismatch (manifest {head_want}, actual {})",
            rep.head
        ));
    }
    Ok(format!(
        "bundle OK — {checked} files match, journal chain intact ({} events, head {})",
        rep.events,
        truncate_chars(&rep.head, 12)
    ))
}

// ---------- OTLP/JSON export ----------

/// Build one OTLP/JSON attribute object `{"key": k, "value": {"stringValue": v}}`.
fn attr_str(k: &str, v: &str) -> Json {
    jobj(vec![
        ("key", jstr(k)),
        ("value", jobj(vec![("stringValue", jstr(v))])),
    ])
}

/// Build one OTLP/JSON attribute object with an `intValue` (encoded as a JSON
/// string per the OTLP/JSON protobuf mapping for 64-bit integers).
fn attr_int(k: &str, v: i64) -> Json {
    jobj(vec![
        ("key", jstr(k)),
        ("value", jobj(vec![("intValue", jstr(&v.to_string()))])),
    ])
}

/// Derive a 32-hex-char OTLP traceId from the journal head hash.
///
/// A valid soma hash is already 64 hex chars; we take the first 32.
/// For the genesis sentinel we sha256 the project name + "otlp".
fn trace_id_from_head(head: &str, project: &str) -> String {
    if head == "genesis" || head.is_empty() {
        let seed = format!("{project}:otlp");
        crate::sha256::sha256_hex(seed.as_bytes())[..32].to_string()
    } else {
        head[..head.len().min(32)].to_string()
    }
}

/// Convert `ts` (milliseconds since epoch) into an OTLP unix-nanosecond
/// string.  OTLP/JSON encodes 64-bit timestamps as strings.
fn ts_to_unix_nano_str(ts_ms: i64) -> String {
    let nanos: i64 = ts_ms.saturating_mul(1_000_000);
    nanos.to_string()
}

/// Infer a `gen_ai.operation.name` from the soma event kind, if applicable.
///
/// Only `model.call` and `skill.run` events that clearly involved a provider
/// call are mapped to the `"chat"` operation so akmon treats them as
/// `ProviderCall` events.  All other events map to `None` (no GenAI op).
fn genai_operation(kind: &str) -> Option<&'static str> {
    match kind {
        "model.call" | "model.ask" => Some("chat"),
        _ => None,
    }
}

/// Export the soma journal as an OTLP/JSON `ExportTraceServiceRequest`.
///
/// One span is emitted per journal event (soma's hash chain becomes the span
/// parent chain).  The output file is written to `out` when supplied, otherwise
/// to `exports/<project>-<stamp>.otlp.json` inside the project directory.
///
/// The export itself is journaled as an `"export.bundle"` event with
/// `"format":"otlp"` so the chain stays self-describing.
pub fn export_otlp(c: &Ctx, out: Option<&str>) -> R<std::path::PathBuf> {
    // Verify the chain before doing anything (same discipline as export()).
    let report = c.journal().verify()?;
    if !report.ok {
        let (line, why) = report.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!(
            "journal failed verification at line {line}: {why} — refusing to export"
        ));
    }

    let stamp = {
        let p = crate::util::utc_parts(crate::util::now_ms());
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            p.year, p.month, p.day, p.hour, p.minute, p.second
        )
    };
    let file_name = format!("{}-{stamp}.otlp.json", c.name());
    // `--out` specifies the full output FILE path (not a directory).
    // When absent the file is placed in exports/<project>-<stamp>.otlp.json.
    let out_path = match out {
        Some(o) => crate::util::expand_home(o),
        None => {
            let exports = c.root.join("exports");
            ensure_dir(&exports)?;
            exports.join(&file_name)
        }
    };
    // If the caller passed a directory, place the file inside it.
    let out_path = if out_path.is_dir() {
        out_path.join(&file_name)
    } else {
        out_path
    };

    // Gate a user-supplied --out by writable_paths and journal the decision
    // (R3) — same boundary the bundle export enforces, BEFORE any write.
    if out.is_some() {
        gate_out_path(c, &out_path)?;
    }

    let trace_id = trace_id_from_head(&report.head, &c.name());
    // gen_ai.conversation.id: deterministic — sha256(project + head)[..16]
    let conv_id = {
        let seed = format!("{}:{}", c.name(), report.head);
        crate::sha256::sha256_hex(seed.as_bytes())[..16].to_string()
    };

    let mut spans: Vec<Json> = Vec::new();
    // Track prev hash so we can set parentSpanId per span.
    // Genesis event has no parent (parentSpanId = "").
    let mut prev_span_id = String::new();

    c.journal().for_each(|ev| {
        let hash = ev.str_of("hash");
        if hash.is_empty() {
            return;
        }
        // spanId: first 16 hex chars of the event hash (unique within the trace).
        let span_id = hash[..hash.len().min(16)].to_string();
        let prev = ev.str_of("prev");
        // parentSpanId: first 16 hex chars of prev hash, or "" for genesis.
        let parent_span_id = if prev == "genesis" || prev.is_empty() {
            String::new()
        } else {
            prev[..prev.len().min(16)].to_string()
        };

        let kind = ev.str_of("kind");
        let ts_ms = ev.i_of("ts");
        let data = ev.get("data").cloned().unwrap_or(Json::Null);

        let start_nano = ts_to_unix_nano_str(ts_ms);
        // Use duration_ms from data when present, else zero-duration.
        let duration_ms = data.i_of("ms").max(data.i_of("duration_ms")).max(0);
        let end_nano = if duration_ms > 0 {
            ts_to_unix_nano_str(ts_ms + duration_ms)
        } else {
            start_nano.clone()
        };

        // Build attributes.
        let mut attrs: Vec<Json> = Vec::new();
        // GenAI semconv attributes (mandatory for akmon to recognise provider calls).
        if let Some(op) = genai_operation(&kind) {
            attrs.push(attr_str("gen_ai.operation.name", op));
        }
        // Derive provider/model from data when present.
        let provider = data.str_of("provider");
        let model = data.str_of("model");
        if !provider.is_empty() {
            attrs.push(attr_str("gen_ai.provider.name", &provider));
        }
        if !model.is_empty() {
            attrs.push(attr_str("gen_ai.request.model", &model));
        }
        // Token counts when available.
        let input_tokens = data.i_of("input_tokens");
        let output_tokens = data.i_of("output_tokens");
        if input_tokens > 0 {
            attrs.push(attr_int("gen_ai.usage.input_tokens", input_tokens));
        }
        if output_tokens > 0 {
            attrs.push(attr_int("gen_ai.usage.output_tokens", output_tokens));
        }
        // Conversation id — same for all spans so they form one session.
        attrs.push(attr_str("gen_ai.conversation.id", &conv_id));
        // soma-native attributes (faithful payload).
        attrs.push(attr_str("soma.kind", &kind));
        attrs.push(attr_str("soma.id", &ev.str_of("id")));
        attrs.push(attr_str("soma.hash", &hash));
        if !prev.is_empty() {
            attrs.push(attr_str("soma.prev", &prev));
        }
        // Flatten scalar data fields as soma.data.<key> string attributes.
        if let Json::Obj(pairs) = &data {
            for (k, v) in pairs {
                match v {
                    Json::Str(s) => {
                        attrs.push(attr_str(&format!("soma.data.{k}"), s));
                    }
                    Json::Num(n) => {
                        attrs.push(attr_str(
                            &format!("soma.data.{k}"),
                            &{
                                let i = *n as i64;
                                if i as f64 == *n { i.to_string() } else { n.to_string() }
                            },
                        ));
                    }
                    Json::Bool(b) => {
                        attrs.push(attr_str(
                            &format!("soma.data.{k}"),
                            if *b { "true" } else { "false" },
                        ));
                    }
                    _ => {} // skip nested objects/arrays
                }
            }
        }

        // Span name = event kind (human-readable, what akmon shows in traces).
        let span = jobj(vec![
            ("traceId", jstr(&trace_id)),
            ("spanId", jstr(&span_id)),
            ("parentSpanId", jstr(&parent_span_id)),
            ("name", jstr(&kind)),
            ("kind", jint(3)), // SPAN_KIND_CLIENT
            ("startTimeUnixNano", jstr(&start_nano)),
            ("endTimeUnixNano", jstr(&end_nano)),
            ("attributes", jarr(attrs)),
        ]);
        spans.push(span);
        prev_span_id = span_id;
    })?;

    if spans.is_empty() {
        return Err("journal is empty — nothing to export".into());
    }

    // Wrap in ExportTraceServiceRequest.
    let resource_attrs = vec![
        attr_str("service.name", "soma"),
        attr_str("soma.project", &c.name()),
        attr_str("soma.version", SOMA_VERSION),
    ];
    let doc = jobj(vec![("resourceSpans", jarr(vec![
        jobj(vec![
            ("resource", jobj(vec![
                ("attributes", jarr(resource_attrs)),
            ])),
            ("scopeSpans", jarr(vec![
                jobj(vec![
                    ("scope", jobj(vec![
                        ("name", jstr("soma")),
                        ("version", jstr(SOMA_VERSION)),
                    ])),
                    ("spans", jarr(spans)),
                ]),
            ])),
        ]),
    ]))]);

    let json_bytes = doc.pretty().into_bytes();
    atomic_write(&out_path, &json_bytes)?;

    // Journal the export as "export.bundle" with format:"otlp" (no new kind).
    c.log(
        "export.bundle",
        jobj(vec![
            ("dir", jstr(out_path.to_string_lossy().as_ref())),
            ("events", jint(report.events as i64)),
            ("head", jstr(&report.head)),
            ("format", jstr("otlp")),
        ]),
    )?;

    Ok(out_path)
}

fn verify_instructions(bundle: &str, anchors: &[Json]) -> String {
    let mut out = format!(
        "# Verifying this bundle ({bundle})\n\n\
With soma: `soma export verify <this directory>`\n\n\
Without soma:\n\n\
1. Check file integrity — for each entry in `manifest.json` under `files`:\n\
   `shasum -a 256 <file>` must equal its `sha256` value.\n\
2. Check the event chain in `events.jsonl` — for each line, `hash` is\n\
   SHA-256 of the line with the `,\"hash\":\"…\"` member removed, and `prev`\n\
   equals the `hash` of the previous line (first line: `prev` = \"genesis\").\n\
   `manifest.json:journal_head` equals the `hash` of the last line.\n\n\
Any edited, removed, or reordered event breaks the chain from that point on.\n"
    );
    if !anchors.is_empty() {
        out.push_str(
            "\n## ANCHORS — RFC 3161 third-party timestamps (anchors/)\n\n\
The journal head was timestamped by a Time Stamp Authority: even the\n\
operator cannot backdate the chain below an anchor. For each anchor the\n\
message is the 64-char head hash as ASCII; the imprint is its SHA-256\n\
(`printf <head> | shasum -a 256`). Both the query (.tsq) and the full DER\n\
response (.tsr) are in `anchors/`. Any openssl that can verify sha256\n\
responses works (macOS stock LibreSSL 3.3+ included; it just can't CREATE\n\
sha256 queries — not needed here).\n\n\
Fetch the root CA of the TSA that answered (the `url` in the matching\n\
`journal.anchor` event):\n\n\
```\n\
# FreeTSA (https://freetsa.org/tsr):\n\
curl -sSO https://freetsa.org/files/cacert.pem\n\
# DigiCert (http://timestamp.digicert.com) — root is DigiCert Trusted Root G4:\n\
curl -sSO https://cacerts.digicert.com/DigiCertTrustedRootG4.crt.pem\n\
```\n\n",
        );
        for a in anchors {
            let head = a.str_of("head");
            let imprint = sha256_hex(head.as_bytes());
            let tsr = a.str_of("tsr_file");
            let tsq = a.str_of("tsq_file");
            out.push_str(&format!(
                "### anchor seq {} (head {}, TSA {})\n\n\
```\n\
# inspect: genTime, serial, policy OID, the echoed imprint\n\
openssl ts -reply -in anchors/{tsr} -text\n\
# verify the timestamp signature over this journal head's imprint:\n\
openssl ts -verify -digest {imprint} -in anchors/{tsr} -CAfile <root.pem>\n\
# strongest check — verifies against the archived query itself:\n\
openssl ts -verify -queryfile anchors/{tsq} -in anchors/{tsr} -CAfile <root.pem>\n\
```\n\n\
Expect `Verification: OK`. A wrong digest MUST fail with\n\
`message imprint mismatch` — try it as a negative control.\n\n",
                a.i_of("seq"),
                truncate_chars(&head, 16),
                a.str_of("url"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::jobj as o;
    use crate::project::testutil::temp_ctx;

    #[test]
    fn export_then_verify_roundtrip() {
        let (base, c) = temp_ctx();
        for i in 0..3 {
            c.log("test.event", o(vec![("i", jint(i))])).unwrap();
        }
        let dir = export(&c, None).unwrap();
        assert!(dir.join("manifest.json").is_file());
        assert!(dir.join("events.jsonl").is_file());
        assert!(dir.join("VERIFY.md").is_file());
        let msg = verify_bundle(&dir).unwrap();
        assert!(msg.contains("bundle OK"), "{msg}");

        // Tamper with a bundled file → verification fails.
        let p = dir.join("config.json");
        let mut s = std::fs::read_to_string(&p).unwrap();
        s.push_str("\n// tampered");
        std::fs::write(&p, s).unwrap();
        assert!(verify_bundle(&dir).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn export_bundles_anchors_with_verify_instructions() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![("i", jint(1))])).unwrap();
        // Synthesize a granted anchor (no network): files + journal record.
        let rep = c.journal().verify().unwrap();
        let head = rep.head.clone();
        let imprint = {
            let mut h = crate::sha256::Sha256::new();
            h.update(head.as_bytes());
            h.finish()
        };
        let dir = c.dir.join("anchors");
        ensure_dir(&dir).unwrap();
        let tsq = crate::anchor::build_tsq(&imprint);
        let tsr = {
            let mut token = vec![0x30, 0x22, 0x04, 0x20];
            token.extend_from_slice(&imprint);
            let mut body = vec![0x30, 0x03, 0x02, 0x01, 0x00];
            body.extend_from_slice(&token);
            let mut buf = vec![0x30, body.len() as u8];
            buf.extend_from_slice(&body);
            buf
        };
        std::fs::write(dir.join("anchor-2-test.tsq"), &tsq).unwrap();
        std::fs::write(dir.join("anchor-2-test.tsr"), &tsr).unwrap();
        c.log(
            "journal.anchor",
            o(vec![
                ("seq", jint(rep.events as i64)),
                ("head", jstr(&head)),
                ("url", jstr("https://freetsa.org/tsr")),
                ("tsq_file", jstr("anchor-2-test.tsq")),
                ("tsr_file", jstr("anchor-2-test.tsr")),
                ("tsr_sha256", jstr(&sha256_hex(&tsr))),
                ("status", jstr("granted")),
            ]),
        )
        .unwrap();

        let bundle = export(&c, None).unwrap();
        // anchors copied into the bundle and listed in the manifest
        assert!(bundle.join("anchors/anchor-2-test.tsq").is_file());
        assert!(bundle.join("anchors/anchor-2-test.tsr").is_file());
        let manifest =
            crate::json::parse(&std::fs::read_to_string(bundle.join("manifest.json")).unwrap())
                .unwrap();
        let files = manifest.get("files").unwrap();
        assert!(files.get("anchors/anchor-2-test.tsr").is_some());
        assert!(files.get("anchors/anchor-2-test.tsq").is_some());
        // VERIFY.md gains the ANCHORS section with the exact openssl commands
        let verify_md = std::fs::read_to_string(bundle.join("VERIFY.md")).unwrap();
        assert!(verify_md.contains("ANCHORS"), "{verify_md}");
        let imprint_hex = sha256_hex(head.as_bytes());
        assert!(verify_md.contains(&format!(
            "openssl ts -verify -digest {imprint_hex} -in anchors/anchor-2-test.tsr"
        )));
        assert!(verify_md.contains("-queryfile anchors/anchor-2-test.tsq"));
        assert!(verify_md.contains("https://freetsa.org/files/cacert.pem"));
        assert!(verify_md.contains("DigiCertTrustedRootG4.crt.pem"));
        // bundle still verifies end-to-end (anchor files hashed in manifest)
        assert!(verify_bundle(&bundle).unwrap().contains("bundle OK"));
        // tampering with a bundled anchor file is caught
        std::fs::write(bundle.join("anchors/anchor-2-test.tsr"), b"evil").unwrap();
        assert!(verify_bundle(&bundle).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn export_refuses_broken_journal() {
        let (base, c) = temp_ctx();
        c.log("a", o(vec![])).unwrap();
        c.log("b", o(vec![])).unwrap();
        // Corrupt the journal.
        let jp = c.dir.join("events.jsonl");
        let content = std::fs::read_to_string(&jp).unwrap().replace("\"a\"", "\"x\"");
        std::fs::write(&jp, content).unwrap();
        let err = export(&c, None).unwrap_err();
        assert!(err.contains("refusing to export"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- OTLP export unit tests ----------

    /// Helper to get the first string attribute value from a span by key.
    fn span_attr_str<'a>(span: &'a crate::json::Json, key: &str) -> &'a str {
        span.get("attributes")
            .and_then(|a| a.arr())
            .and_then(|arr| {
                arr.iter().find(|kv| kv.str_of("key") == key)
            })
            .and_then(|kv| kv.get("value"))
            .and_then(|v| v.get("stringValue"))
            .and_then(|v| v.s())
            .unwrap_or("")
    }

    #[test]
    fn otlp_export_document_shape_three_events() {
        let (base, c) = temp_ctx();
        // project.init is already in the journal; add 2 more synthetic events.
        c.log("test.alpha", o(vec![("x", jint(1))])).unwrap();
        c.log("test.beta", o(vec![("x", jint(2))])).unwrap();

        // Use the default exports dir (inside-project, ungated) so the span
        // count stays exactly the 3 logged events — a gated --out would add a
        // policy.decision event to the chain (that path is covered separately
        // by *_out_outside_writable_paths_* and subformat_out_inside_*).
        let path = export_otlp(&c, None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let doc = crate::json::parse(&content).expect("output is valid JSON");

        // Top-level structure: resourceSpans array.
        let resource_spans = doc.get("resourceSpans").and_then(|r| r.arr()).expect("resourceSpans");
        assert!(!resource_spans.is_empty(), "resourceSpans must not be empty");

        let rs = &resource_spans[0];
        // Resource attributes must include service.name = "soma".
        let res_attrs = rs.get("resource").and_then(|r| r.get("attributes")).and_then(|a| a.arr()).expect("resource.attributes");
        let svc_name = res_attrs.iter()
            .find(|kv| kv.str_of("key") == "service.name")
            .and_then(|kv| kv.get("value"))
            .and_then(|v| v.get("stringValue"))
            .and_then(|v| v.s())
            .unwrap_or("");
        assert_eq!(svc_name, "soma", "service.name attribute");

        // scopeSpans and spans.
        let scope_spans = rs.get("scopeSpans").and_then(|s| s.arr()).expect("scopeSpans");
        let spans = scope_spans[0].get("spans").and_then(|s| s.arr()).expect("spans");

        // 3 journal events = 3 spans (project.init + 2 test events).
        assert_eq!(spans.len(), 3, "expected 3 spans, got {}", spans.len());

        // First span: genesis parent → parentSpanId must be "".
        let first = &spans[0];
        assert_eq!(first.str_of("parentSpanId"), "", "genesis span must have no parent");
        let first_span_id = first.str_of("spanId");
        assert!(!first_span_id.is_empty(), "spanId must not be empty");

        // Second span: parent = first span's spanId.
        let second = &spans[1];
        assert_eq!(
            second.str_of("parentSpanId"),
            first_span_id,
            "second span parent must equal first span id"
        );

        // Third span: parent = second span's spanId.
        let third = &spans[2];
        assert_eq!(
            third.str_of("parentSpanId"),
            second.str_of("spanId"),
            "third span parent must equal second span id"
        );

        // span name = event kind.
        assert_eq!(first.str_of("name"), "project.init");
        assert_eq!(second.str_of("name"), "test.alpha");
        assert_eq!(third.str_of("name"), "test.beta");

        // startTimeUnixNano must be a non-empty string (OTLP/JSON encoding).
        let nano = first.str_of("startTimeUnixNano");
        assert!(!nano.is_empty(), "startTimeUnixNano must be a string");
        assert!(nano.parse::<i64>().is_ok(), "startTimeUnixNano must be numeric string, got {nano}");

        // soma.kind attribute must match span name.
        assert_eq!(span_attr_str(first, "soma.kind"), "project.init");
        assert_eq!(span_attr_str(second, "soma.kind"), "test.alpha");

        // soma.hash and soma.prev must be present on spans 2+.
        assert!(!span_attr_str(second, "soma.hash").is_empty(), "soma.hash missing");
        assert!(!span_attr_str(second, "soma.prev").is_empty(), "soma.prev missing");

        // gen_ai.conversation.id must be present on all spans.
        assert!(!span_attr_str(first, "gen_ai.conversation.id").is_empty(), "conv id missing");

        // traceId must be the same for all spans.
        let trace_id = first.str_of("traceId");
        assert!(!trace_id.is_empty(), "traceId empty");
        for span in spans {
            assert_eq!(span.str_of("traceId"), trace_id, "all spans must share traceId");
        }

        // Export is journaled as export.bundle with format:otlp.
        let tail = c.journal().tail(5).unwrap();
        let export_ev = tail.iter().find(|e| e.str_of("kind") == "export.bundle").expect("export.bundle event");
        assert_eq!(export_ev.get("data").unwrap().str_of("format"), "otlp");

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn otlp_export_refuses_broken_journal() {
        let (base, c) = temp_ctx();
        c.log("a", o(vec![])).unwrap();
        // Corrupt.
        let jp = c.dir.join("events.jsonl");
        let content = std::fs::read_to_string(&jp).unwrap().replace("\"a\"", "\"z\"");
        std::fs::write(&jp, content).unwrap();
        let err = export_otlp(&c, Some("/tmp")).unwrap_err();
        assert!(err.contains("refusing to export"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Integration test: run the full soma->akmon->agef-verify pipeline.
    ///
    /// Opt-in and portable: point it at local akmon/agef-verify builds with
    /// `SOMA_AKMON_BIN` and `SOMA_AGEF_VERIFY_BIN`. Skips cleanly when those
    /// are unset or missing (same pattern as the MCP python3-skip tests).
    #[test]
    fn otlp_export_full_pipeline_integration() {
        let (Ok(akmon), Ok(agef_verify)) = (
            std::env::var("SOMA_AKMON_BIN"),
            std::env::var("SOMA_AGEF_VERIFY_BIN"),
        ) else {
            eprintln!("skipping otlp pipeline integration test: set SOMA_AKMON_BIN and SOMA_AGEF_VERIFY_BIN");
            return;
        };
        let akmon = std::path::Path::new(&akmon);
        let agef_verify = std::path::Path::new(&agef_verify);
        if !akmon.is_file() || !agef_verify.is_file() {
            eprintln!("skipping otlp pipeline integration test: akmon binaries not present");
            return;
        }

        let (base, c) = temp_ctx();
        // Seed the journal with a few events including a model.call kind.
        c.log("test.event", o(vec![("n", jint(1))])).unwrap();
        c.log("model.call", o(vec![
            ("provider", jstr("anthropic")),
            ("model", jstr("claude-3-5-sonnet-20241022")),
            ("input_tokens", jint(50)),
            ("output_tokens", jint(20)),
            ("ms", jint(1200)),
        ])).unwrap();
        c.log("test.event", o(vec![("n", jint(3))])).unwrap();

        // Step 1: soma export otlp (write inside the project root, which is in
        // writable_paths so the --out path gate allows it).
        let trace_out_dir = c.root.join("otlp_pipeline_out");
        std::fs::create_dir_all(&trace_out_dir).unwrap();
        let trace_path = export_otlp(&c, Some(&trace_out_dir.to_string_lossy())).unwrap();
        assert!(trace_path.is_file(), "trace file must exist: {}", trace_path.display());

        // Step 2: akmon otel import
        let tmpj = tempdir_for_test();
        let import_out = std::process::Command::new(akmon)
            .args(["otel", "import"])
            .arg(&trace_path)
            .arg("--journal")
            .arg(&tmpj)
            .arg("--format")
            .arg("json")
            .output()
            .expect("akmon otel import must run");
        assert!(
            import_out.status.success(),
            "akmon otel import failed (exit {}): {}",
            import_out.status,
            String::from_utf8_lossy(&import_out.stderr)
        );

        // Parse session_id from JSON output.
        let report_str = String::from_utf8_lossy(&import_out.stdout);
        let report = crate::json::parse(&report_str).expect("import report is valid JSON");
        let session_id = report.str_of("session_id");
        assert!(!session_id.is_empty(), "session_id must not be empty");

        // Step 3: akmon bundle export (write bundle next to trace file in same dir).
        let bundle_path = trace_out_dir.join("session.akmon");
        let bundle_out = std::process::Command::new(akmon)
            .args(["bundle", "export"])
            .arg(&session_id)
            .arg("--journal")
            .arg(&tmpj)
            .arg("--output")
            .arg(&bundle_path)
            .output()
            .expect("akmon bundle export must run");
        assert!(
            bundle_out.status.success(),
            "akmon bundle export failed (exit {}): {}",
            bundle_out.status,
            String::from_utf8_lossy(&bundle_out.stderr)
        );

        // Step 4: agef-verify
        let verify_out = std::process::Command::new(agef_verify)
            .arg(&bundle_path)
            .output()
            .expect("agef-verify must run");
        assert!(
            verify_out.status.success(),
            "agef-verify failed (exit {}):\n  stdout: {}\n  stderr: {}",
            verify_out.status,
            String::from_utf8_lossy(&verify_out.stdout),
            String::from_utf8_lossy(&verify_out.stderr)
        );

        // Cleanup: tmpj journal dir + the test temp tree (trace + bundle live in base).
        std::fs::remove_dir_all(&tmpj).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- sub-format --out path-gating (R3 uniformity) ----------

    /// True iff a policy.decision with allowed:false was journaled for an
    /// export.write subject — proof the refusal landed on the chain.
    fn refused_export_write_journaled(c: &Ctx) -> bool {
        let mut hit = false;
        c.journal()
            .for_each(|ev| {
                if ev.str_of("kind") == "policy.decision" {
                    if let Some(d) = ev.get("data") {
                        if d.str_of("subject").starts_with("export.write:") && !d.b_of("allowed") {
                            hit = true;
                        }
                    }
                }
            })
            .unwrap();
        hit
    }

    #[test]
    fn otlp_out_outside_writable_paths_refused_and_journaled() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let err = export_otlp(&c, Some("/etc/x")).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        assert!(!std::path::Path::new("/etc/x").exists(), "must not have written");
        assert!(refused_export_write_journaled(&c), "refusal must be journaled");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn attestation_out_outside_writable_paths_refused_and_journaled() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let err = crate::attest::export_attestation(&c, None, Some("/etc/x")).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        assert!(!std::path::Path::new("/etc/x").exists(), "must not have written");
        assert!(refused_export_write_journaled(&c), "refusal must be journaled");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn aiact_out_outside_writable_paths_refused_and_journaled() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let err = crate::aiact::export_aiact(&c, Some("/etc/x.md")).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        assert!(!std::path::Path::new("/etc/x.md").exists(), "must not have written");
        assert!(!std::path::Path::new("/etc/x.json").exists(), "sibling must not exist");
        assert!(refused_export_write_journaled(&c), "refusal must be journaled");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn subformat_out_inside_writable_paths_succeeds() {
        // A --out inside the default writable_paths ({project}/*) is allowed for
        // every sub-format — the gate must not be a blanket block.
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let otlp = export_otlp(&c, Some(&c.root.join("o.otlp.json").to_string_lossy())).unwrap();
        assert!(otlp.is_file());
        let att = crate::attest::export_attestation(
            &c,
            None,
            Some(&c.root.join("a.json").to_string_lossy()),
        )
        .unwrap();
        assert!(att.is_file());
        let (md, json) =
            crate::aiact::export_aiact(&c, Some(&c.root.join("x.md").to_string_lossy())).unwrap();
        assert!(md.is_file() && json.is_file());
        std::fs::remove_dir_all(&base).ok();
    }

    /// Create a temporary directory path (does not create the dir itself —
    /// akmon creates it if needed, or we pass the path to it).
    fn tempdir_for_test() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "soma_test_journal_{}_{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        p
    }
}
