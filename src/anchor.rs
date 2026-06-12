//! D10 — journal anchoring: RFC 3161 timestamps over the chain head.
//!
//! `soma anchor now` proves the journal existed (with its current head) at a
//! point in time certified by a third-party Time Stamp Authority — killing
//! the "operator regenerates and re-signs the whole chain" attack: even the
//! operator cannot backdate a TSA signature.
//!
//! The message is the 64-char lowercase hex head hash as ASCII bytes; the
//! messageImprint is SHA-256 of that. The TimeStampReq is a fixed 59-byte
//! DER template (research-verified live against FreeTSA, DigiCert, Sectigo
//! and Apple on 2026-06-12) where only the 32 hash bytes vary. The POST goes
//! through the system `curl` (models.rs precedent — the OS owns TLS). Both
//! the query (.tsq) and the full DER response (.tsr) are archived under
//! `.soma/anchors/`; the journal itself is the index (`journal.anchor`).
//!
//! Honest limits: anchoring proves existence-at-time of the head hash, not
//! the truth of event contents. Verification trust comes from the TSA's
//! signature; full third-party verification (openssl + the TSA's root CA)
//! is documented in every export bundle's VERIFY.md.

use crate::json::{jbool, jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::sha256::{sha256_hex, Sha256};
use crate::util::*;
use std::path::{Path, PathBuf};

pub const DEFAULT_TSA_URL: &str = "https://freetsa.org/tsr";

/// TSA hosts the cloud presets allow (local-only never does). DigiCert is
/// the production-proven alternate (plain http; trust is in the signature,
/// not the transport). Sectigo asks ≥15s between scripted requests — usable
/// via --url, but deliberately not a default.
pub const TSA_HOSTS: [&str; 2] = ["freetsa.org", "timestamp.digicert.com"];

// ---------- DER TimeStampReq encoder ----------

/// Fixed prefix of the 59-byte TimeStampReq: SEQUENCE(57) | version INTEGER 1
/// | messageImprint SEQUENCE(49) | AlgorithmIdentifier SEQUENCE(13) with the
/// SHA-256 OID 2.16.840.1.101.3.4.2.1 + NULL params | OCTET STRING header.
const TSQ_PREFIX: [u8; 24] = [
    0x30, 0x39, // TimeStampReq SEQUENCE, 57 content bytes
    0x02, 0x01, 0x01, // version INTEGER 1
    0x30, 0x31, // messageImprint SEQUENCE, 49 content bytes
    0x30, 0x0d, // AlgorithmIdentifier SEQUENCE, 13 content bytes
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, // OID sha256
    0x05, 0x00, // parameters NULL
    0x04, 0x20, // hashedMessage OCTET STRING, 32 bytes follow
];
/// certReq BOOLEAN TRUE — the TSA embeds its signer cert, so years later a
/// stranger verifies with just the root CA (no hunt for a rotated leaf).
/// nonce and reqPolicy are absent by omission (no placeholder bytes).
const TSQ_SUFFIX: [u8; 3] = [0x01, 0x01, 0xff];

/// Build the 59-byte TimeStampReq for a SHA-256 imprint. Only bytes 24..56
/// vary between requests; everything else is the verified fixed template.
pub fn build_tsq(imprint: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(59);
    out.extend_from_slice(&TSQ_PREFIX);
    out.extend_from_slice(imprint);
    out.extend_from_slice(&TSQ_SUFFIX);
    out
}

// ---------- DER TimeStampResp parser (honest TLV walk, no fixed offsets) ----------

/// Read one DER TLV header at `pos` → (tag, content_len, content_start).
/// Handles short and long-form lengths; rejects indefinite (0x80) encoding.
fn tlv(buf: &[u8], pos: usize) -> Result<(u8, usize, usize), String> {
    if pos + 2 > buf.len() {
        return Err(format!("truncated DER: no TLV header at byte {pos}"));
    }
    let tag = buf[pos];
    let l0 = buf[pos + 1];
    if l0 == 0x80 {
        return Err(format!("indefinite-length DER at byte {pos} — rejected"));
    }
    if l0 & 0x80 == 0 {
        return Ok((tag, l0 as usize, pos + 2));
    }
    let n = (l0 & 0x7f) as usize;
    if n == 0 || n > 4 {
        return Err(format!("unsupported DER length-of-length {n} at byte {pos}"));
    }
    if pos + 2 + n > buf.len() {
        return Err(format!("truncated DER length at byte {pos}"));
    }
    let mut len = 0usize;
    for i in 0..n {
        len = (len << 8) | buf[pos + 2 + i] as usize;
    }
    Ok((tag, len, pos + 2 + n))
}

fn pki_status_name(status: i64) -> &'static str {
    match status {
        0 => "granted",
        1 => "grantedWithMods",
        2 => "rejection",
        3 => "waiting",
        4 => "revocationWarning",
        5 => "revocationNotification",
        _ => "unknown",
    }
}

/// Extract PKIStatus from a TimeStampResp (RFC 3161 §2.4.2):
/// TimeStampResp SEQ { PKIStatusInfo SEQ { status INTEGER, ... }, token? }.
/// A TLV walk — never fixed offsets: Apple inserts a statusString
/// ("Operation Okay") into PKIStatusInfo, which breaks byte-pattern matching.
pub fn parse_pki_status(buf: &[u8]) -> Result<i64, String> {
    let (tag, olen, ostart) = tlv(buf, 0)?;
    if tag != 0x30 {
        return Err(format!("response is not a DER SEQUENCE (tag 0x{tag:02x})"));
    }
    // The outer SEQUENCE must declare a length that its content fits inside —
    // a length claiming to extend past the buffer is malformed.
    if ostart + olen > buf.len() {
        return Err("truncated TimeStampResp SEQUENCE".into());
    }
    let (tag, silen, sistart) = tlv(buf, ostart)?;
    if tag != 0x30 {
        return Err(format!("PKIStatusInfo is not a SEQUENCE (tag 0x{tag:02x})"));
    }
    if sistart + silen > buf.len() {
        return Err("truncated PKIStatusInfo".into());
    }
    // PKIStatusInfo must sit within the outer SEQUENCE's declared content.
    if sistart + silen > ostart + olen {
        return Err("PKIStatusInfo overruns the TimeStampResp SEQUENCE".into());
    }
    let (tag, ilen, istart) = tlv(buf, sistart)?;
    if tag != 0x02 {
        return Err(format!("PKIStatus is not an INTEGER (tag 0x{tag:02x})"));
    }
    if ilen == 0 || ilen > 4 || istart + ilen > buf.len() {
        return Err(format!("PKIStatus INTEGER length {ilen} out of range"));
    }
    // The status INTEGER must be contained by the PKIStatusInfo SEQUENCE — a
    // length field claiming to extend past the declared PKIStatusInfo length is
    // a containment violation, not a valid status.
    if istart + ilen > sistart + silen {
        return Err("PKIStatus INTEGER overruns PKIStatusInfo".into());
    }
    let mut v: i64 = 0;
    for i in 0..ilen {
        v = (v << 8) | buf[istart + i] as i64;
    }
    Ok(v)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Accept a TimeStampResp iff PKIStatus is granted(0)/grantedWithMods(1) AND
/// the response echoes our imprint (`04 20 || hash` occurs inside TSTInfo).
/// HTTP 200 does NOT imply granted — TSAs return 200 with rejection bodies.
pub fn check_granted(buf: &[u8], imprint: &[u8; 32]) -> Result<i64, String> {
    let status = parse_pki_status(buf)?;
    if status != 0 && status != 1 {
        return Err(format!(
            "TSA did not grant a token: PKIStatus {status} ({})",
            pki_status_name(status)
        ));
    }
    let mut needle = Vec::with_capacity(34);
    needle.extend_from_slice(&[0x04, 0x20]);
    needle.extend_from_slice(imprint);
    if !contains(buf, &needle) {
        return Err(
            "granted response does not echo our messageImprint (04 20 || hash needle missing) — the TSA stamped a different hash"
                .into(),
        );
    }
    Ok(status)
}

// ---------- config + url helpers ----------

pub fn tsa_url(c: &Ctx) -> String {
    let u = c
        .config
        .get("anchor")
        .map(|a| a.str_of("tsa_url"))
        .unwrap_or_default();
    if u.is_empty() {
        DEFAULT_TSA_URL.into()
    } else {
        u
    }
}

pub fn auto_mode(c: &Ctx) -> String {
    let m = c
        .config
        .get("anchor")
        .map(|a| a.str_of("auto"))
        .unwrap_or_default();
    if m.is_empty() {
        "off".into()
    } else {
        m
    }
}

/// Extract the host from an http(s) URL for the network gate. Anything
/// unexpected stays IN the host string (e.g. userinfo tricks like
/// `https://a@b/`), so an unmatched allowlist denies — fail closed.
pub fn host_of_url(url: &str) -> R<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("anchor: unsupported URL '{url}' — must start with http:// or https://"))?;
    let hostport = rest.split('/').next().unwrap_or("");
    if hostport.contains('@') {
        return Err(format!("anchor: URL '{url}' contains userinfo — refused"));
    }
    let host = hostport.split(':').next().unwrap_or("");
    if host.is_empty() {
        return Err(format!("anchor: no host in URL '{url}'"));
    }
    Ok(host.to_string())
}

fn anchors_dir(c: &Ctx) -> PathBuf {
    c.dir.join("anchors")
}

fn imprint_of_head(head: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.finish()
}

fn journal_anchor(
    c: &Ctx,
    seq: i64,
    head: &str,
    url: &str,
    tsq_file: &str,
    tsr_file: &str,
    tsr_sha256: &str,
    status: &str,
    reason: Option<&str>,
) -> R<Json> {
    let mut data = jobj(vec![
        ("seq", jint(seq)),
        ("head", jstr(head)),
        ("url", jstr(url)),
        ("tsq_file", jstr(tsq_file)),
        ("tsr_file", jstr(tsr_file)),
        ("tsr_sha256", jstr(tsr_sha256)),
        ("status", jstr(status)),
    ]);
    if let Some(r) = reason {
        data.set("reason", jstr(r));
    }
    let ev = c.log("journal.anchor", data)?;
    Ok(ev.get("data").cloned().unwrap_or(Json::Null))
}

// ---------- anchor now ----------

/// Anchor the current journal head at a TSA. Chain-first: a broken chain
/// refuses before anything else (export precedent). The network gate fires
/// at egress — BEFORE any curl — and the decision is journaled either way.
/// Failed attempts (policy, curl, non-granted) are journaled with
/// status:"failed" + reason and the command errors (non-zero exit).
pub fn now(c: &Ctx, url_override: Option<&str>) -> R<Json> {
    // 1. Verify the chain BEFORE anchoring — a timestamp over a broken
    //    chain would be a plausible-looking artifact of nothing.
    let report = c.journal().verify()?;
    if !report.ok {
        let (line, why) = report.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!(
            "journal failed verification at line {line}: {why} — refusing to anchor"
        ));
    }
    if report.events == 0 {
        return Err("journal is empty — nothing to anchor".into());
    }
    let seq = report.events as i64;
    let head = report.head.clone();
    let url = url_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| tsa_url(c));
    let host = host_of_url(&url)?;

    // 2. Network gate at the egress boundary (models.rs gate precedent).
    let dec = c.policy.check_network(&host);
    c.log("policy.decision", dec.to_json(&format!("network:{host}")))?;
    if !dec.allowed() {
        let reason = format!("network blocked by policy for {host} ({})", dec.rule());
        // The failed attempt is journaled too, so `anchor.auto: daily`
        // backs off until the next day instead of re-attempting every tick.
        journal_anchor(c, seq, &head, &url, "", "", "", "failed", Some(&reason))?;
        return Err(reason);
    }

    // 3. Build + archive the query (the .tsq enables the strongest
    //    third-party check: openssl ts -verify -queryfile).
    let imprint = imprint_of_head(&head);
    let tsq = build_tsq(&imprint);
    let stamp = {
        let p = utc_parts(now_ms());
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            p.year, p.month, p.day, p.hour, p.minute, p.second
        )
    };
    let dir = anchors_dir(c);
    ensure_dir(&dir)?;
    let tsq_name = format!("anchor-{seq}-{stamp}.tsq");
    let tsr_name = format!("anchor-{seq}-{stamp}.tsr");
    let tsq_path = dir.join(&tsq_name);
    let tsr_path = dir.join(&tsr_name);
    atomic_write(&tsq_path, &tsq)?;

    let fail = |reason: String, tsr_file: &str, tsr_sha: &str| -> R<Json> {
        journal_anchor(c, seq, &head, &url, &tsq_name, tsr_file, tsr_sha, "failed", Some(&reason))?;
        Err(reason)
    };

    // 4. POST via system curl. --data-binary (NOT -d, which corrupts binary
    //    bodies); --fail → non-zero exit on HTTP ≥ 400.
    let out = std::process::Command::new("curl")
        .args([
            "-sS",
            "--fail",
            "--max-time",
            "20",
            "-H",
            "Content-Type: application/timestamp-query",
            "--data-binary",
            &format!("@{}", tsq_path.display()),
            "-o",
            &tsr_path.to_string_lossy(),
            &url,
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return fail(format!("could not run curl: {e}"), "", ""),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return fail(format!("curl failed against {url}: {err}"), "", "");
    }

    // 5. Parse the response: PKIStatus + imprint echo. HTTP 200 ≠ granted.
    let tsr_bytes = match std::fs::read(&tsr_path) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return fail(format!("{url} returned an empty response"), "", ""),
        Err(e) => return fail(format!("response file unreadable: {e}"), "", ""),
    };
    let tsr_sha = sha256_hex(&tsr_bytes);
    if let Err(e) = check_granted(&tsr_bytes, &imprint) {
        // Keep the .tsr — a rejection body is evidence of the attempt.
        return fail(e, &tsr_name, &tsr_sha);
    }

    journal_anchor(c, seq, &head, &url, &tsq_name, &tsr_name, &tsr_sha, "granted", None)
}

// ---------- anchor list ----------

/// All journal.anchor events, in chain order (full events as stored).
pub fn list(c: &Ctx) -> R<Vec<Json>> {
    let mut v = Vec::new();
    c.journal().for_each(|ev| {
        if ev.str_of("kind") == "journal.anchor" {
            v.push(ev.clone());
        }
    })?;
    Ok(v)
}

// ---------- anchor verify ----------

/// Recompute the chain hash AT `seq`: stream events.jsonl, re-verify every
/// line's `prev` link and content hash up to and including line `seq`, and
/// return that line's hash. Errors on tampering or a too-short journal.
pub fn chain_head_at(journal_path: &Path, seq: usize) -> R<String> {
    if seq == 0 {
        return Err("seq must be ≥ 1".into());
    }
    let mut prev = crate::events::GENESIS.to_string();
    let mut processed = 0usize;
    let mut bad: Option<String> = None;
    for_each_line(journal_path, |line| {
        if bad.is_some() || processed >= seq {
            return Ok(());
        }
        let line_no = processed + 1;
        let ev = match crate::json::parse(line) {
            Ok(ev) => ev,
            Err(e) => {
                bad = Some(format!("unparseable event at line {line_no}: {e}"));
                return Ok(());
            }
        };
        if ev.str_of("prev") != prev {
            bad = Some(format!("broken chain at line {line_no}: prev mismatch"));
            return Ok(());
        }
        let core = match &ev {
            Json::Obj(pairs) => {
                Json::Obj(pairs.iter().filter(|(k, _)| k != "hash").cloned().collect())
            }
            _ => {
                bad = Some(format!("event at line {line_no} is not an object"));
                return Ok(());
            }
        };
        let recomputed = sha256_hex(core.to_string().as_bytes());
        if recomputed != ev.str_of("hash") {
            bad = Some(format!("content hash mismatch at line {line_no} (line edited)"));
            return Ok(());
        }
        prev = recomputed;
        processed += 1;
        Ok(())
    })?;
    if let Some(b) = bad {
        return Err(b);
    }
    if processed < seq {
        return Err(format!(
            "journal has only {processed} events but the anchor covers seq {seq}"
        ));
    }
    Ok(prev)
}

/// Best-effort third-party check via the system openssl (optional — checked,
/// never required; stock macOS LibreSSL ≥ 3.3 verifies sha256 responses).
/// Full verification needs the TSA's root CA: we use `.soma/anchors/cacert.pem`
/// when the operator dropped one there, else the system bundle (which holds
/// DigiCert's root but not FreeTSA's), else run without and report honestly.
fn openssl_verify(tsr_path: &Path, imprint_hex: &str, dir: &Path) -> (bool, bool, String) {
    let probe = std::process::Command::new("openssl").arg("version").output();
    let version = match probe {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return (false, false, "skipped — no runnable system openssl (optional check)".into()),
    };
    let mut args: Vec<String> = vec![
        "ts".into(),
        "-verify".into(),
        "-digest".into(),
        imprint_hex.into(),
        "-in".into(),
        tsr_path.to_string_lossy().to_string(),
    ];
    let ca_local = dir.join("cacert.pem");
    let ca_sys = Path::new("/etc/ssl/cert.pem");
    let ca = if ca_local.is_file() {
        Some(ca_local)
    } else if ca_sys.is_file() {
        Some(ca_sys.to_path_buf())
    } else {
        None
    };
    if let Some(ca) = &ca {
        args.push("-CAfile".into());
        args.push(ca.to_string_lossy().to_string());
    }
    let out = std::process::Command::new("openssl").args(&args).output();
    match out {
        Ok(o) => {
            let txt = format!(
                "{} {}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let ok = o.status.success() && txt.contains("Verification: OK");
            let verdict = if ok {
                "Verification: OK".to_string()
            } else {
                let first = txt
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("verification failed")
                    .trim()
                    .to_string();
                format!(
                    "{first} — full third-party verification needs the TSA's root CA (drop it at .soma/anchors/cacert.pem, or use the ANCHORS commands in an export's VERIFY.md)"
                )
            };
            (
                true,
                ok,
                format!("`openssl ts -verify -digest {imprint_hex} -in {}{}` ({version}) → {verdict}",
                    tsr_path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                    ca.map(|c| format!(" -CAfile {}", c.display())).unwrap_or_default()),
            )
        }
        Err(e) => (false, false, format!("skipped — could not run openssl: {e}")),
    }
}

/// Verify one anchor record. Three checks, reported individually:
///   1. chain    — recompute the chain hash at the anchored seq (required)
///   2. tsr_file — stored response matches the journaled sha256, still
///                 parses as granted, and echoes the imprint (required)
///   3. openssl  — best-effort signature verification (advisory: it cannot
///                 succeed without the TSA's root CA on disk)
/// `ok` in the report covers the required checks only.
pub fn verify_anchor(c: &Ctx, d: &Json) -> Json {
    let seq = d.i_of("seq");
    let head = d.str_of("head");
    let imprint = imprint_of_head(&head);
    let imprint_hex = sha256_hex(head.as_bytes());

    // 1. chain at seq
    let (chain_ok, chain_note) = match chain_head_at(&c.journal().path, seq.max(0) as usize) {
        Ok(h) if h == head => (
            true,
            format!("recomputed chain hash at seq {seq} matches the journaled head"),
        ),
        Ok(h) => (
            false,
            format!(
                "recomputed head {} ≠ journaled {} — the chain below this anchor changed",
                truncate_chars(&h, 16),
                truncate_chars(&head, 16)
            ),
        ),
        Err(e) => (false, e),
    };

    // 2. tsr file integrity + content
    let dir = anchors_dir(c);
    let tsr_name = d.str_of("tsr_file");
    let tsr_path = dir.join(&tsr_name);
    let (file_ok, file_note) = if tsr_name.is_empty() {
        (false, "no tsr_file in the anchor record".to_string())
    } else {
        match std::fs::read(&tsr_path) {
            Ok(bytes) => {
                let got = sha256_hex(&bytes);
                if got != d.str_of("tsr_sha256") {
                    (
                        false,
                        format!(
                            "{tsr_name}: sha256 mismatch (journal {}, file {})",
                            truncate_chars(&d.str_of("tsr_sha256"), 16),
                            truncate_chars(&got, 16)
                        ),
                    )
                } else {
                    match check_granted(&bytes, &imprint) {
                        Ok(status) => (
                            true,
                            format!(
                                "{tsr_name}: sha256 matches the journal record; PKIStatus {} and imprint echoed",
                                pki_status_name(status)
                            ),
                        ),
                        Err(e) => (false, format!("{tsr_name}: sha256 matches but {e}")),
                    }
                }
            }
            Err(e) => (false, format!("cannot read {}: {e}", tsr_path.display())),
        }
    };

    // 3. openssl, best-effort
    let (ossl_ran, ossl_ok, ossl_note) = if file_ok {
        openssl_verify(&tsr_path, &imprint_hex, &dir)
    } else {
        (false, false, "skipped — tsr file check failed".to_string())
    };

    let required_ok = chain_ok && file_ok;
    jobj(vec![
        ("seq", jint(seq)),
        ("head", jstr(&head)),
        ("ok", jbool(required_ok)),
        (
            "checks",
            jobj(vec![
                (
                    "chain",
                    jobj(vec![
                        ("ran", jbool(true)),
                        ("ok", jbool(chain_ok)),
                        ("note", jstr(&chain_note)),
                    ]),
                ),
                (
                    "tsr_file",
                    jobj(vec![
                        ("ran", jbool(true)),
                        ("ok", jbool(file_ok)),
                        ("note", jstr(&file_note)),
                    ]),
                ),
                (
                    "openssl",
                    jobj(vec![
                        ("ran", jbool(ossl_ran)),
                        ("ok", jbool(ossl_ok)),
                        ("note", jstr(&ossl_note)),
                    ]),
                ),
            ]),
        ),
    ])
}

/// Human rendering of one verify report.
pub fn render_verify(report: &Json) -> String {
    let checks = report.get("checks").cloned().unwrap_or_else(|| jobj(vec![]));
    let line = |name: &str| -> String {
        let ch = checks.get(name).cloned().unwrap_or_else(|| jobj(vec![]));
        let state = if !ch.b_of("ran") {
            "skipped"
        } else if ch.b_of("ok") {
            "ok"
        } else {
            "FAILED"
        };
        format!("  {name:<9} {state:<8} {}\n", ch.str_of("note"))
    };
    let mut out = format!(
        "anchor seq {} (head {})\n",
        report.i_of("seq"),
        truncate_chars(&report.str_of("head"), 16)
    );
    out.push_str(&line("chain"));
    out.push_str(&line("tsr_file"));
    out.push_str(&line("openssl"));
    out.push_str(&format!(
        "  result: {} (chain + tsr_file are required; openssl is best-effort)\n",
        if report.b_of("ok") { "OK" } else { "FAILED" }
    ));
    out
}

// ---------- anchor.auto (tick integration) ----------

const DAY_MS: i64 = 24 * 3600 * 1000;

/// Is a daily auto-anchor due at `now`? Pure decision logic — no egress.
/// Any journaled attempt (granted OR failed) stamps the clock, so a failure
/// is not retried until the next day.
pub fn auto_due(c: &Ctx, now: i64) -> bool {
    if auto_mode(c) != "daily" {
        return false;
    }
    let mut last: i64 = 0;
    let _ = c.journal().for_each(|ev| {
        if ev.str_of("kind") == "journal.anchor" {
            last = last.max(ev.i_of("ts"));
        }
    });
    now - last > DAY_MS
}

/// tick hook: attempt one anchor when due. Failures are reported in the tick
/// output (and journaled by `now()`), never fatal to the tick itself.
pub fn auto_anchor(c: &Ctx) -> Option<String> {
    if !auto_due(c, now_ms()) {
        return None;
    }
    Some(match now(c, None) {
        Ok(d) => format!(
            "anchor.auto: journal head anchored at seq {} via {} (granted)",
            d.i_of("seq"),
            d.str_of("url")
        ),
        Err(e) => format!("anchor.auto: attempt failed — {e} (next attempt in ~24h)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    fn hexdec(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Research test vector (2026-06-12): TSQ for the all-zeros SHA-256
    /// digest, cmp-identical to OpenSSL 3.6.0 `ts -query -digest 00…00
    /// -sha256 -cert -no_nonce`, accepted live by 4 TSAs.
    const VECTOR_HEX: &str = "30390201013031300d06096086480165030402010500042000000000000000000000000000000000000000000000000000000000000000000101ff";

    #[test]
    fn tsq_matches_openssl_verified_vector() {
        let tsq = build_tsq(&[0u8; 32]);
        assert_eq!(tsq.len(), 59);
        assert_eq!(tsq, hexdec(VECTOR_HEX), "encoder must be byte-identical to the verified vector");
    }

    #[test]
    fn tsq_only_hash_bytes_vary() {
        let mut imprint = [0u8; 32];
        for (i, b) in imprint.iter_mut().enumerate() {
            *b = i as u8;
        }
        let tsq = build_tsq(&imprint);
        assert_eq!(tsq.len(), 59);
        let zero = build_tsq(&[0u8; 32]);
        assert_eq!(tsq[..24], zero[..24], "prefix is fixed");
        assert_eq!(tsq[24..56], imprint, "bytes 24..56 are the imprint");
        assert_eq!(tsq[56..], zero[56..], "suffix (certReq TRUE) is fixed");
    }

    /// Minimal granted response: SEQ{ SEQ{ INT 0 }, fake-token containing
    /// `04 20 || hash` } — the shape FreeTSA/DigiCert/Sectigo return.
    fn synthetic_granted(imprint: &[u8; 32]) -> Vec<u8> {
        let mut token = vec![0x30, 0x22, 0x04, 0x20];
        token.extend_from_slice(imprint);
        let mut body = vec![0x30, 0x03, 0x02, 0x01, 0x00];
        body.extend_from_slice(&token);
        let mut out = vec![0x30, body.len() as u8];
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parser_minimal_granted_response() {
        let imprint = [7u8; 32];
        let buf = synthetic_granted(&imprint);
        assert_eq!(parse_pki_status(&buf).unwrap(), 0);
        assert_eq!(check_granted(&buf, &imprint).unwrap(), 0);
    }

    #[test]
    fn parser_apple_status_string_shape() {
        // Apple: PKIStatusInfo = SEQ{ INT 0, SEQ{ UTF8 "Operation Okay" } } —
        // a fixed-offset 30 03 02 01 00 match would fail here.
        let imprint = [9u8; 32];
        let msg = b"Operation Okay";
        let mut si = vec![0x02, 0x01, 0x00, 0x30, (msg.len() + 2) as u8, 0x0c, msg.len() as u8];
        si.extend_from_slice(msg);
        let mut token = vec![0x30, 0x22, 0x04, 0x20];
        token.extend_from_slice(&imprint);
        let mut body = vec![0x30, si.len() as u8];
        body.extend_from_slice(&si);
        body.extend_from_slice(&token);
        let mut buf = vec![0x30, body.len() as u8];
        buf.extend_from_slice(&body);
        assert_eq!(parse_pki_status(&buf).unwrap(), 0);
        assert_eq!(check_granted(&buf, &imprint).unwrap(), 0);
        // grantedWithMods(1) also passes
        let mut buf1 = buf.clone();
        let pos = buf1.windows(3).position(|w| w == [0x02, 0x01, 0x00]).unwrap();
        buf1[pos + 2] = 0x01;
        assert_eq!(check_granted(&buf1, &imprint).unwrap(), 1);
    }

    #[test]
    fn parser_rejection_status() {
        // SEQ{ SEQ{ INT 2 } } — HTTP 200 with a rejection body.
        let buf = vec![0x30, 0x05, 0x30, 0x03, 0x02, 0x01, 0x02];
        assert_eq!(parse_pki_status(&buf).unwrap(), 2);
        let err = check_granted(&buf, &[0u8; 32]).unwrap_err();
        assert!(err.contains("PKIStatus 2"), "{err}");
        assert!(err.contains("rejection"), "{err}");
    }

    #[test]
    fn parser_truncated_garbage_rejected() {
        let imprint = [3u8; 32];
        // empty / single byte / truncated header
        assert!(parse_pki_status(&[]).is_err());
        assert!(parse_pki_status(&[0x30]).is_err());
        // wrong outer tag
        assert!(parse_pki_status(&[0x04, 0x02, 0x00, 0x00]).is_err());
        // indefinite length rejected
        assert!(parse_pki_status(&[0x30, 0x80, 0x30, 0x03, 0x02, 0x01, 0x00]).is_err());
        // long-form length claiming more bytes than exist
        assert!(parse_pki_status(&[0x30, 0x82, 0xff]).is_err());
        // status integer truncated
        assert!(parse_pki_status(&[0x30, 0x04, 0x30, 0x02, 0x02, 0x04]).is_err());
        // valid long-form outer length still parses (real TSAs use 30 82 …)
        let short = synthetic_granted(&imprint);
        let content = &short[2..];
        let mut long = vec![0x30, 0x82, 0x00, content.len() as u8];
        long.extend_from_slice(content);
        assert_eq!(parse_pki_status(&long).unwrap(), 0);
    }

    #[test]
    fn parser_status_integer_overrunning_pkistatusinfo_rejected() {
        // F5 — containment: the status INTEGER declares a length (3) that
        // extends past the PKIStatusInfo SEQUENCE's declared length (2), yet
        // the bytes exist in the buffer so the buffer-bounds check alone would
        // miss it. Must be rejected as a containment violation.
        //
        // Layout: 30 09 | 30 02 | 02 03 00 00 00 | 00 00
        //   outer SEQ len 9, PKIStatusInfo SEQ len 2 (= `02 01` worth),
        //   but the INTEGER inside claims 3 content bytes.
        let buf = vec![0x30, 0x09, 0x30, 0x02, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00];
        let err = parse_pki_status(&buf).expect_err("INTEGER overrunning PKIStatusInfo must reject");
        assert!(err.contains("overruns PKIStatusInfo"), "{err}");

        // And the same idea one level up: PKIStatusInfo overrunning the outer
        // SEQUENCE (outer len 3 can't hold a 5-byte PKIStatusInfo). Buffer is
        // padded so the bytes exist.
        let buf = vec![0x30, 0x03, 0x30, 0x05, 0x02, 0x01, 0x00, 0x00, 0x00];
        let err = parse_pki_status(&buf).expect_err("PKIStatusInfo overrunning outer SEQ must reject");
        assert!(
            err.contains("overruns the TimeStampResp SEQUENCE") || err.contains("truncated"),
            "{err}"
        );
    }

    #[test]
    fn needle_required_even_when_granted() {
        let buf = synthetic_granted(&[1u8; 32]);
        let err = check_granted(&buf, &[2u8; 32]).unwrap_err();
        assert!(err.contains("messageImprint"), "{err}");
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of_url("https://freetsa.org/tsr").unwrap(), "freetsa.org");
        assert_eq!(host_of_url("http://timestamp.digicert.com").unwrap(), "timestamp.digicert.com");
        assert_eq!(host_of_url("http://h:8080/p").unwrap(), "h");
        assert!(host_of_url("ftp://x").is_err());
        assert!(host_of_url("https:///nope").is_err());
        assert!(host_of_url("https://evil.com@freetsa.org/tsr").is_err());
    }

    #[test]
    fn anchor_now_refused_under_local_only() {
        let (base, c) = temp_ctx();
        // default policy: allow_network=false, localhost-only allowlist
        let err = now(&c, None).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        let tail = c.journal().tail(10).unwrap();
        // the refusal is journaled exactly like other network refusals…
        let dec = tail
            .iter()
            .find(|e| e.str_of("kind") == "policy.decision"
                && e.get("data").unwrap().str_of("subject") == "network:freetsa.org")
            .expect("policy.decision for the anchor egress");
        assert!(!dec.get("data").unwrap().b_of("allowed"));
        // …and the failed attempt is on the chain (daily auto backs off).
        let fail = tail
            .iter()
            .find(|e| e.str_of("kind") == "journal.anchor")
            .expect("journal.anchor failed event");
        let d = fail.get("data").unwrap();
        assert_eq!(d.str_of("status"), "failed");
        assert!(d.str_of("reason").contains("blocked by policy"));
        // nothing was sent: no anchors dir / no .tsq written
        assert!(!c.dir.join("anchors").is_dir());
        // chain still intact after the refusal events
        assert!(c.journal().verify().unwrap().ok);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anchor_now_refuses_broken_chain() {
        let (base, c) = temp_ctx();
        c.log("a", jobj(vec![])).unwrap();
        let jp = c.journal().path.clone();
        let content = std::fs::read_to_string(&jp).unwrap().replace("\"a\"", "\"x\"");
        std::fs::write(&jp, content).unwrap();
        let err = now(&c, None).unwrap_err();
        assert!(err.contains("refusing to anchor"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn chain_head_at_matches_full_verify() {
        let (base, c) = temp_ctx();
        for i in 0..4 {
            c.log("test.event", jobj(vec![("n", jint(i))])).unwrap();
        }
        let rep = c.journal().verify().unwrap();
        assert!(rep.ok);
        let path = c.journal().path.clone();
        // full-length recompute == verify head
        assert_eq!(chain_head_at(&path, rep.events).unwrap(), rep.head);
        // mid-chain recompute == that line's own hash field
        let lines = std::fs::read_to_string(&path).unwrap();
        let line3 = lines.lines().nth(2).unwrap();
        let ev3 = crate::json::parse(line3).unwrap();
        assert_eq!(chain_head_at(&path, 3).unwrap(), ev3.str_of("hash"));
        // out of range / zero
        assert!(chain_head_at(&path, rep.events + 10).is_err());
        assert!(chain_head_at(&path, 0).is_err());
        // tampering below the requested seq is detected
        let tampered = lines.replacen("test.event", "evil.event", 1);
        std::fs::write(&path, tampered).unwrap();
        assert!(chain_head_at(&path, 3).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    /// Full verify logic over a synthetic (no-network) granted anchor.
    #[test]
    fn verify_synthetic_granted_anchor() {
        let (base, c) = temp_ctx();
        for i in 0..3 {
            c.log("test.event", jobj(vec![("n", jint(i))])).unwrap();
        }
        let rep = c.journal().verify().unwrap();
        let (seq, head) = (rep.events as i64, rep.head.clone());
        let imprint = imprint_of_head(&head);
        let tsr_bytes = synthetic_granted(&imprint);
        let dir = c.dir.join("anchors");
        ensure_dir(&dir).unwrap();
        std::fs::write(dir.join("anchor-test.tsq"), build_tsq(&imprint)).unwrap();
        std::fs::write(dir.join("anchor-test.tsr"), &tsr_bytes).unwrap();
        let data = journal_anchor(
            &c, seq, &head, "https://example.test/tsr",
            "anchor-test.tsq", "anchor-test.tsr", &sha256_hex(&tsr_bytes),
            "granted", None,
        )
        .unwrap();

        let report = verify_anchor(&c, &data);
        assert!(report.b_of("ok"), "required checks must pass: {}", report.pretty());
        let checks = report.get("checks").unwrap();
        assert!(checks.get("chain").unwrap().b_of("ok"));
        assert!(checks.get("tsr_file").unwrap().b_of("ok"));
        // openssl is best-effort: a synthetic token can never signature-verify,
        // and the check must report rather than flip the result.
        assert!(checks.get("openssl").unwrap().get("note").is_some());

        // human rendering names all three checks
        let text = render_verify(&report);
        assert!(text.contains("chain") && text.contains("tsr_file") && text.contains("openssl"));

        // tamper the stored response → required check fails
        std::fs::write(dir.join("anchor-test.tsr"), b"corrupted").unwrap();
        let report2 = verify_anchor(&c, &data);
        assert!(!report2.b_of("ok"));
        assert!(!report2.get("checks").unwrap().get("tsr_file").unwrap().b_of("ok"));
        assert!(report2.get("checks").unwrap().get("chain").unwrap().b_of("ok"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn list_returns_anchor_events_in_order() {
        let (base, c) = temp_ctx();
        assert!(list(&c).unwrap().is_empty());
        journal_anchor(&c, 1, "h1", "u", "q", "r", "s", "failed", Some("x")).unwrap();
        journal_anchor(&c, 3, "h3", "u", "q", "r", "s", "granted", None).unwrap();
        let l = list(&c).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].get("data").unwrap().i_of("seq"), 1);
        assert_eq!(l[1].get("data").unwrap().str_of("status"), "granted");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn auto_due_decision_logic() {
        let (base, mut c) = temp_ctx();
        let t0 = now_ms();
        // default: anchor.auto absent → off → never due
        assert!(!auto_due(&c, t0));
        // daily + no attempts yet → due
        c.config.set(
            "anchor",
            jobj(vec![("tsa_url", jstr(DEFAULT_TSA_URL)), ("auto", jstr("daily"))]),
        );
        c.save_config().unwrap();
        assert!(auto_due(&c, t0));
        // a journaled attempt (even failed) stamps the clock → not due for 24h
        journal_anchor(&c, 1, "h", "u", "", "", "", "failed", Some("policy")).unwrap();
        assert!(!auto_due(&c, now_ms() + 3600 * 1000), "1h later: not due");
        assert!(auto_due(&c, now_ms() + 25 * 3600 * 1000), "25h later: due again");
        std::fs::remove_dir_all(&base).ok();
    }
}
