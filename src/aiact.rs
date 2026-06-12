//! D11 — `soma export eu-ai-act`: auditor-ready EU AI Act Article 12 logging
//! annex generated from the append-only journal.
//!
//! Writes `<name>.md` (plain, greppable markdown — VERIFY.md's tone) plus a
//! machine-readable `<name>.json` sibling with stable keys (docs/JSON-API.md).
//! Refuses on a broken chain (export precedent). The document describes the
//! logging CAPABILITY and the records actually captured — it is NOT a
//! conformity assessment and performs no Article 6 classification; the eight
//! caveats are on page 1, not buried. Quotations are from the authentic OJ
//! text only (CELEX:32024R1689), research-verified 2026-06-12.

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::project::{Ctx, SOMA_VERSION};
use crate::util::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Rendered wherever an operator-supplied field is absent — the annex must be
/// honest about its own gaps, never silently omit them.
pub const PLACEHOLDER: &str = "[NOT PROVIDED — operator must complete]";

/// Operator-supplied fields, set via `soma config set aiact.<key> <value>`
/// (validated at the mutation boundary in cli.rs, like anchor.*).
pub const AIACT_KEYS: [&str; 5] = [
    "system",
    "provider",
    "deployer",
    "intended_purpose",
    "classification",
];

/// The operator's Article 6 position is a structured choice, not free text —
/// the generator renders each value as the matching legal wording.
pub const CLASSIFICATIONS: [&str; 4] = [
    "high-risk-6-1",
    "high-risk-6-2",
    "not-high-risk-6-3",
    "out-of-scope",
];

/// The generator's knowledge of the law is frozen at the research date, not
/// at generation time — a document generated later must not claim fresher
/// legal knowledge than the binary actually has.
pub const LEGAL_SNAPSHOT: &str = "2026-06-12";

const CELEX: &str = "CELEX:32024R1689";
const ELI: &str = "http://data.europa.eu/eli/reg/2024/1689/oj";

// Application dates — BOTH printed wherever dates appear (caveat C7).
const IN_FORCE_ANNEX3: &str = "2026-08-02"; // Art 113: general application
const IN_FORCE_ANNEX1: &str = "2027-08-02"; // Art 113(c): Art 6(1) systems
const OMNIBUS_ANNEX3: &str = "2027-12-02"; // AI Omnibus (provisional)
const OMNIBUS_ANNEX1: &str = "2028-08-02"; // AI Omnibus (provisional)
const OMNIBUS_STATUS: &str =
    "provisional Parliament–Council agreement May 2026, pending OJ publication";

pub(crate) struct KindStat {
    pub(crate) count: i64,
    pub(crate) first: String,
    pub(crate) last: String,
}

struct PeriodRow {
    source: &'static str, // "wrap" | "skill.run" | "goal.run"
    label: String,
    start: String,
    end: Option<String>,
    duration_ms: Option<i64>,
    exit: Option<i64>,
    ok: Option<bool>,
    timed_out: bool,
}

/// Everything the annex needs, collected in ONE journal pass (the export.rs
/// precedent: extend the closure, don't re-stream). Shared with attest.rs
/// (D12): the attestation predicate reuses this walk instead of duplicating
/// the journal-streaming logic.
pub(crate) struct Collected {
    pub(crate) first_iso: String,
    first_ts: i64,
    pub(crate) last_iso: String,
    pub(crate) kinds: BTreeMap<String, KindStat>,
    periods: Vec<PeriodRow>,
    /// Data objects of granted journal.anchor events, verbatim as stored
    /// ({seq, head, url, tsq_file, tsr_file, tsr_sha256, status} — D10).
    pub(crate) anchors: Vec<Json>,
    denials: i64, // policy.decision events with allowed:false
}

pub(crate) fn collect(c: &Ctx) -> R<Collected> {
    let mut col = Collected {
        first_iso: String::new(),
        first_ts: 0,
        last_iso: String::new(),
        kinds: BTreeMap::new(),
        periods: Vec::new(),
        anchors: Vec::new(),
        denials: 0,
    };
    // Pending wrap.start receipts awaiting their wrap.end. Paired on
    // (label, pid) so concurrent same-label wraps don't mis-attribute START
    // time (F9); pid is carried on both wrap.start and wrap.end (D9). When a
    // wrap.end has no pid (legacy event) we fall back to most-recent-by-label.
    // A leftover start = session crashed or still running.
    let mut pending: Vec<(String, i64, String)> = Vec::new(); // (label, pid, start iso)
    c.journal().for_each(|ev| {
        let kind = ev.str_of("kind");
        let iso = ev.str_of("iso");
        let ts = ev.i_of("ts");
        if col.first_iso.is_empty() {
            col.first_iso = iso.clone();
            col.first_ts = ts;
        }
        col.last_iso = iso.clone();
        let st = col.kinds.entry(kind.clone()).or_insert(KindStat {
            count: 0,
            first: iso.clone(),
            last: iso.clone(),
        });
        st.count += 1;
        st.last = iso.clone();
        let data = ev.get("data").cloned().unwrap_or(Json::Null);
        match kind.as_str() {
            "wrap.start" => {
                // pid defaults to 0 when absent (legacy events) — paired below
                // by label as before in that case.
                pending.push((data.str_of("label"), data.i_of("pid"), iso.clone()))
            }
            "wrap.end" => {
                let label = data.str_of("label");
                let pid = data.i_of("pid");
                // Prefer an exact (label, pid) match; only fall back to the
                // most-recent same-label start when pid is missing (0) on
                // either side (legacy wrap.end with no pid field).
                let start = pending
                    .iter()
                    .rposition(|(l, p, _)| *l == label && pid != 0 && *p == pid)
                    .or_else(|| pending.iter().rposition(|(l, _, _)| *l == label))
                    .map(|i| pending.remove(i).2)
                    .unwrap_or_default();
                col.periods.push(PeriodRow {
                    source: "wrap",
                    label,
                    start,
                    end: Some(iso.clone()),
                    duration_ms: Some(data.i_of("duration_ms")),
                    exit: Some(data.i_of("exit")),
                    ok: None,
                    timed_out: data.b_of("timed_out"),
                });
            }
            "skill.run" => {
                // skill.run is journaled at completion with its duration —
                // start is derived (ts - ms).
                let ms = data.i_of("ms");
                col.periods.push(PeriodRow {
                    source: "skill.run",
                    label: data.str_of("name"),
                    start: iso8601(ts - ms),
                    end: Some(iso.clone()),
                    duration_ms: Some(ms),
                    exit: None,
                    ok: Some(data.b_of("ok")),
                    timed_out: false,
                });
            }
            "goal.run" => {
                // goal.run is journaled at initiation; per-step outcomes are
                // the goal.step events (section 4). No end receipt exists —
                // stated, not invented.
                let title = data.str_of("title");
                col.periods.push(PeriodRow {
                    source: "goal.run",
                    label: if title.is_empty() { data.str_of("goal") } else { title },
                    start: iso.clone(),
                    end: None,
                    duration_ms: None,
                    exit: None,
                    ok: None,
                    timed_out: false,
                });
            }
            "journal.anchor" => {
                if data.str_of("status") == "granted" {
                    col.anchors.push(data.clone());
                }
            }
            "policy.decision" => {
                if !data.b_of("allowed") {
                    col.denials += 1;
                }
            }
            _ => {}
        }
    })?;
    // Unmatched wrap.start receipts: still running, or soma was killed — the
    // crash-safe start receipt is itself evidence and must appear.
    for (label, _pid, start) in pending {
        col.periods.push(PeriodRow {
            source: "wrap",
            label,
            start,
            end: None,
            duration_ms: None,
            exit: None,
            ok: None,
            timed_out: false,
        });
    }
    col.periods.sort_by(|a, b| a.start.cmp(&b.start));
    Ok(col)
}

/// Article 12(2) mapping for a journal event kind:
/// (a) identifies Art 79(1) risk situations / substantial modification,
/// (b) feeds Art 72 post-market monitoring,
/// (c) supports Art 26(5) deployer monitoring.
/// Kinds mapping to none are state-keeping and claim no Art 12(2) role.
fn art12_map(kind: &str) -> (bool, bool, bool) {
    let a = matches!(
        kind,
        "policy.decision" // gate denials and allows — the risk boundary itself
            | "policy.change"
            | "preset.apply"
            | "config.change"
            | "skill.issue" // anomalies / run failures
            | "mcp.add"
            | "mcp.remove"
    );
    let b = matches!(
        kind,
        "skill.run"
            | "skill.issue"
            | "goal.run"
            | "goal.done"
            | "model.call"
            | "wrap.end"
            | "cron.run"
            | "tick.run"
            | "optimize.run"
            | "proposal.new"
    );
    let c = matches!(
        kind,
        "wrap.start"
            | "wrap.end"
            | "skill.run"
            | "goal.run"
            | "goal.step"
            | "goal.done"
            | "model.route"
            | "model.call"
            | "model.ask"
            | "select.explain"
            | "select.rerank"
            | "mcp.rpc"
            | "proposal.apply"
            | "proposal.dismiss"
            | "cron.run"
            | "tick.run"
    );
    (a, b, c)
}

/// Operator-supplied aiact.* value, or the explicit placeholder.
fn operator_field(c: &Ctx, key: &str) -> String {
    let v = c
        .config
        .get("aiact")
        .map(|a| a.str_of(key))
        .unwrap_or_default();
    if v.trim().is_empty() {
        PLACEHOLDER.into()
    } else {
        v
    }
}

fn classification_prose(v: &str) -> String {
    match v {
        "high-risk-6-1" => "high-risk under Article 6(1) — safety component of (or itself) a \
                            product under Annex I Union harmonisation legislation requiring \
                            third-party conformity assessment"
            .into(),
        "high-risk-6-2" => "high-risk under Article 6(2) — intended purpose falls in an \
                            Annex III area"
            .into(),
        "not-high-risk-6-3" => "not high-risk by virtue of the Article 6(3) derogation — the \
                                operator must hold the Article 6(4) documented assessment and \
                                register under Article 49(2)"
            .into(),
        "out-of-scope" => "not high-risk — intended purpose is outside Annex III and the \
                           system is not an Annex I safety component (operator's position)"
            .into(),
        other => other.into(), // placeholder or pre-validation free text, as-is
    }
}

/// The dates block, repeated wherever dates appear (caveat C7).
fn dates_block() -> String {
    format!(
        "Application dates — both sets printed per caveat C7 (legal-status snapshot \
         {LEGAL_SNAPSHOT}):\n\
         - in-force text (Article 113, {CELEX}): Annex III high-risk obligations apply from \
         {IN_FORCE_ANNEX3}; Article 6(1)/Annex I systems from {IN_FORCE_ANNEX1}.\n\
         - AI Omnibus ({OMNIBUS_STATUS}): defers these to {OMNIBUS_ANNEX3} (Annex III) and \
         {OMNIBUS_ANNEX1} (Annex I). Not yet law as of the snapshot — verify before relying \
         on either set.\n"
    )
}

const CAVEAT_CODES: [&str; 8] = ["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"];

fn caveats_block() -> String {
    format!(
        "Read these eight caveats before relying on anything below.\n\n\
- C1 — capability, not conformity. This document describes the system's logging\n\
  capability and the records actually captured in the journal. It is NOT a\n\
  conformity assessment under Article 43, not an EU declaration of conformity\n\
  under Article 47, confers no CE marking under Article 48, and claims no\n\
  presumption of conformity under Article 40 (no harmonised standard is claimed).\n\
- C2 — no classification performed. This document does not classify the system\n\
  as high-risk or not. The Article 6 classification — including any Article 6(3)\n\
  derogation, its Article 6(4) documented assessment and Article 49(2)\n\
  registration — is the operator's own legal responsibility. Most internal\n\
  coding/development agents are not high-risk at all: software development is\n\
  not an Annex III area.\n\
- C3 — Article 12 is one requirement of many. A genuinely high-risk system also\n\
  requires risk management (Art 9), data governance (Art 10), technical\n\
  documentation (Art 11/Annex IV), transparency and instructions for use\n\
  (Art 13), human oversight (Art 14), accuracy/robustness/cybersecurity\n\
  (Art 15), a quality management system (Art 17), registration (Art 49) and\n\
  post-market monitoring (Art 72). Possessing this annex does not by itself\n\
  make any system compliant with the AI Act.\n\
- C4 — not legal advice. No substitute for assessment by qualified counsel or,\n\
  where required, a notified body.\n\
- C5 — completeness is bounded by the journal. This document can only attest to\n\
  events that were actually recorded; it cannot prove the absence of unrecorded\n\
  activity outside the instrumented boundary (section 10).\n\
- C6 — retention shown reflects current configuration. The operator remains\n\
  responsible for meeting the Article 19(1)/26(6) ≥6-month floor and for\n\
  GDPR-compliant handling of any personal data inside logs.\n\
- C7 — application dates are in flux. The in-force text of Regulation (EU)\n\
  2024/1689 applies Annex III high-risk obligations from {IN_FORCE_ANNEX3}\n\
  (Annex I: {IN_FORCE_ANNEX1}), but the provisionally agreed AI Omnibus\n\
  (May 2026, pending OJ publication as of the legal-status snapshot\n\
  {LEGAL_SNAPSHOT}) defers these to {OMNIBUS_ANNEX3} (Annex III) and\n\
  {OMNIBUS_ANNEX1} (Annex I). Both sets are printed throughout; verify the\n\
  current status before relying on either.\n\
- C8 — only the Official Journal is authoritative. Quotations are from the\n\
  authentic OJ text ({CELEX}, ELI {ELI}).\n"
    )
}

fn fmt_opt_ms(v: Option<i64>) -> String {
    v.map(|n| format!("{n} ms")).unwrap_or_else(|| "—".into())
}

/// Escape a free-text value for safe interpolation into a GFM table cell (F7).
/// Operator/agent-controlled text (e.g. a wrap `--label`) could otherwise
/// inject extra columns with `|` or break the table with a newline. The JSON
/// sibling is already structured and needs no escaping.
fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Generate the annex. Returns (markdown path, json path).
pub fn export_aiact(c: &Ctx, out: Option<&str>) -> R<(PathBuf, PathBuf)> {
    // Verify the chain BEFORE writing anything (export precedent) — an annex
    // generated from a broken journal would be a plausible-looking lie.
    let report = c.journal().verify()?;
    if !report.ok {
        let (line, why) = report.first_bad.unwrap_or((0, "unknown".into()));
        return Err(format!(
            "journal failed verification at line {line}: {why} — refusing to export"
        ));
    }
    if report.events == 0 {
        return Err("journal is empty — nothing to export".into());
    }

    let now = now_ms();
    let stamp = {
        let p = utc_parts(now);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            p.year, p.month, p.day, p.hour, p.minute, p.second
        )
    };
    let file_name = format!("{}-aiact-{stamp}.md", c.name());
    // `--out` is the markdown FILE path (otlp precedent); a directory places
    // the default-named file inside it. The .json sibling sits next to it.
    let md_path = match out {
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
    let json_path = md_path.with_extension("json");
    if json_path == md_path {
        return Err(format!(
            "--out {} collides with its .json sibling — use a .md path",
            md_path.display()
        ));
    }

    // Gate a user-supplied --out by writable_paths and journal the decision
    // (R3), BEFORE any write. This export writes BOTH .md and .json siblings;
    // gate each so neither lands outside the boundary.
    if out.is_some() {
        crate::export::gate_out_path(c, &md_path)?;
        crate::export::gate_out_path(c, &json_path)?;
    }

    let col = collect(c)?;
    let generated_at = iso8601(now);
    let age_days = if col.first_ts > 0 {
        (now - col.first_ts) / 86_400_000
    } else {
        0
    };
    let md_name = md_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let json_name = json_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    // Operator-supplied identity (explicit placeholders when missing).
    let system = operator_field(c, "system");
    let provider = operator_field(c, "provider");
    let deployer = operator_field(c, "deployer");
    let purpose = operator_field(c, "intended_purpose");
    let classification = operator_field(c, "classification");

    let md = render_markdown(
        c,
        &report,
        &col,
        &generated_at,
        age_days,
        &md_name,
        &json_name,
        [&system, &provider, &deployer, &purpose, &classification],
    );
    let json_doc = render_json(
        c,
        &report,
        &col,
        &generated_at,
        age_days,
        [&system, &provider, &deployer, &purpose, &classification],
    );

    atomic_write(&md_path, md.as_bytes())?;
    atomic_write(&json_path, json_doc.pretty().as_bytes())?;

    // Journal the generation as "export.bundle" with format:"eu-ai-act" —
    // the otlp precedent; the cockpit kind map is frozen for v6.
    c.log(
        "export.bundle",
        jobj(vec![
            ("dir", jstr(md_path.to_string_lossy().as_ref())),
            ("events", jint(report.events as i64)),
            ("head", jstr(&report.head)),
            ("format", jstr("eu-ai-act")),
        ]),
    )?;

    Ok((md_path, json_path))
}

#[allow(clippy::too_many_arguments)]
fn render_markdown(
    c: &Ctx,
    report: &crate::events::VerifyReport,
    col: &Collected,
    generated_at: &str,
    age_days: i64,
    md_name: &str,
    json_name: &str,
    fields: [&String; 5],
) -> String {
    let [system, provider, deployer, purpose, classification] = fields;
    let project = c.name();
    let head = &report.head;
    let events = report.events;
    let dates = dates_block();
    let mut s = String::new();

    // ---------- page 1: document control + the eight caveats ----------
    s.push_str(&format!(
        "# EU AI Act — Article 12 logging annex: {project}\n\n\
Generated from the append-only soma journal. A machine-readable JSON sibling\n\
({json_name}) carries the same facts with stable keys (docs/JSON-API.md).\n\
Regulation (EU) 2024/1689 (AI Act), {CELEX}, ELI {ELI}.\n\n\
## 0. Document control & caveats\n\n\
- generator: soma {SOMA_VERSION} (format eu-ai-act, version 1)\n\
- project: {project}\n\
- journal HEAD at generation: {head}\n\
- events on the chain: {events} (verified intact at generation — this annex\n\
  refuses to generate from a broken chain)\n\
- generated at: {generated_at} (UTC, local system clock — see section 10)\n\
- legal-status snapshot: {LEGAL_SNAPSHOT} (caveat C7)\n\
- outputs: {md_name} + {json_name}\n\n"
    ));
    s.push_str(&caveats_block());
    s.push('\n');
    s.push_str(&dates);
    s.push('\n');

    // ---------- 1. system identification ----------
    s.push_str(&format!(
        "## 1. System identification\n\n\
Maps to the Annex IV(1) general-description fields of the technical\n\
documentation (Article 11), so this annex can slot into it. Operator-supplied\n\
values come from `soma config set aiact.<key> <value>`; absent values are\n\
stated as such, never invented or silently omitted.\n\n\
- AI system: {system}   (aiact.system)\n\
- provider (Article 3(3)): {provider}   (aiact.provider)\n\
- deployer (Article 3(4)): {deployer}   (aiact.deployer)\n\
- intended purpose: {purpose}   (aiact.intended_purpose)\n\
- logging/governance layer: soma {SOMA_VERSION}, project '{project}',\n\
  journal at .soma/events.jsonl on the deployer's infrastructure\n\n\
Note: an enterprise that builds its agent in-house and puts it into service\n\
for its own use under its own name is BOTH provider and deployer\n\
(Articles 3(3), 3(11)) and owes both obligation stacks.\n\n"
    ));

    // ---------- 2. role & classification ----------
    s.push_str(&format!(
        "## 2. Role & risk-classification status\n\n\
- operator's Article 6 position: {}   (aiact.classification)\n\n\
Classification under Article 6 is the operator's own legal responsibility;\n\
this generator does not perform it (caveat C2). For orientation only:\n\
software development is not an Annex III area, so a coding/development/\n\
internal-workflow agent is not high-risk unless its intended purpose strays\n\
into a listed area — the realistic traps for enterprise agents are Annex III\n\
4(b) (using agent telemetry to monitor and evaluate the performance and\n\
behaviour of employees), 4(a) (recruitment screening), 3 (education\n\
assessment) and 5(b) (creditworthiness). A system that performs profiling of\n\
natural persons is always considered high-risk (Article 6(3), final\n\
subparagraph). A provider invoking the 6(3) derogation must document its\n\
assessment before placing the system on the market or putting it into\n\
service (Article 6(4)) and register under Article 49(2).\n\n{dates}\n",
        classification_prose(classification)
    ));

    // ---------- 3. logging capability ----------
    s.push_str(&format!(
        "## 3. Logging capability (Article 12(1))\n\n\
Article 12(1): \"High-risk AI systems shall technically allow for the\n\
automatic recording of events (logs) over the lifetime of the system.\"\n\
This is a design requirement on the system; retention is Articles 19(1)\n\
and 26(6) (section 7). How the soma journal provides the capability,\n\
whether or not the governed system is high-risk:\n\n\
- automatic: every soma-mediated action (policy decision, skill/goal run,\n\
  model call, wrapped agent session, config/policy mutation, export, anchor)\n\
  is journaled at the moment it happens; no operator action is required.\n\
- lifetime: recording starts at project.init and never stops; the journal is\n\
  append-only and soma has no delete operation for it.\n\
- tamper-evident: each event embeds `prev` (SHA-256 of the previous event)\n\
  and `hash` (SHA-256 of the event itself); editing, deleting or reordering\n\
  any line breaks every later link (section 6).\n\
- redaction-before-write: values whose keys match the policy's redact_keys\n\
  globs are replaced with \"[redacted]\" BEFORE touching disk; wrapped-agent\n\
  output excerpts additionally pass text-level redaction. Secrets and\n\
  personal data matching the patterns never reach the chain.\n\
- control (the \"under their control\" qualifier of Articles 19(1)/26(6)):\n\
  the journal lives under the project root on the deployer's own\n\
  infrastructure; soma transmits nothing home. The logs are therefore under\n\
  the DEPLOYER's control and Article 26(6) is the operative retention duty;\n\
  Article 19(1) binds the provider only for log slices the provider actually\n\
  holds — none, for a local soma installation.\n\
- log interpretation (Article 13(3)(f)): this annex, the VERIFY.md inside\n\
  every `soma export` bundle, and `soma log tail|show|verify` together\n\
  describe how a deployer collects, stores and interprets the logs.\n\n"
    ));

    // ---------- 4. event taxonomy ----------
    s.push_str(
        "## 4. Event taxonomy mapped to Article 12(2)\n\n\
Article 12(2): logging capabilities must enable recording of events relevant\n\
for: (a) \"identifying situations that may result in the high-risk AI system\n\
presenting a risk within the meaning of Article 79(1) or in a substantial\n\
modification\"; (b) \"facilitating the post-market monitoring referred to in\n\
Article 72\"; and (c) \"monitoring the operation of high-risk AI systems\n\
referred to in Article 26(5)\".\n\n\
Every event kind present in this journal, with counts and observation window\n\
(a kind marked \"—\" in all three columns is state-keeping and claims no\n\
Article 12(2) role):\n\n\
| kind | count | first | last | 12(2)(a) risk/modification | 12(2)(b) Art 72 | 12(2)(c) Art 26(5) |\n\
|---|---|---|---|---|---|---|\n",
    );
    for (kind, st) in &col.kinds {
        let (a, b, cc) = art12_map(kind);
        let m = |x: bool| if x { "yes" } else { "—" };
        s.push_str(&format!(
            "| {kind} | {} | {} | {} | {} | {} | {} |\n",
            st.count,
            st.first,
            st.last,
            m(a),
            m(b),
            m(cc)
        ));
    }
    s.push_str(&format!(
        "\nPolicy gate decisions journaled (allowed and refused): refusals on this\n\
chain: {}. Refused mutations and spawns are journaled as policy.decision\n\
with allowed:false — the denial itself is evidence (Article 12(2)(a)).\n\n",
        col.denials
    ));

    // ---------- 5. period-of-use ----------
    s.push_str(
        "## 5. Period-of-use records\n\n\
Article 12(3) imposes its minimum period-of-use logging (\"recording of the\n\
period of each use of the system (start date and time and end date and time\n\
of each use)\") ONLY on Annex III point 1(a) remote biometric identification\n\
systems. soma and the agents it governs are not such systems, so Article\n\
12(3) does not apply here; the table below is provided as good practice and\n\
exceeds what Article 12(2) requires for non-biometric systems.\n\n\
Sessions reconstructed from wrap.start/wrap.end receipt pairs (matched by\n\
label, most recent unmatched start) and from skill.run / goal.run events:\n\n",
    );
    if col.periods.is_empty() {
        s.push_str("(no wrapped sessions, skill runs or goal runs on this chain yet)\n\n");
    } else {
        s.push_str(
            "| start (UTC) | end (UTC) | duration | source | label | outcome |\n\
|---|---|---|---|---|---|\n",
        );
        for p in &col.periods {
            let outcome = match (p.exit, p.ok) {
                (Some(e), _) => {
                    if p.timed_out {
                        format!("exit {e} (timed out)")
                    } else {
                        format!("exit {e}")
                    }
                }
                (None, Some(true)) => "ok".into(),
                (None, Some(false)) => "fail".into(),
                _ => "—".into(),
            };
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                if p.start.is_empty() { "—" } else { &p.start },
                p.end.as_deref().unwrap_or("—"),
                fmt_opt_ms(p.duration_ms),
                md_cell(p.source),
                md_cell(&p.label),
                md_cell(&outcome)
            ));
        }
        s.push_str(
            "\nA wrap.start with no matching wrap.end means the session was still running\n\
or soma was killed — the start receipt is journaled before the child runs,\n\
so even a crash leaves the start on the chain. goal.run is journaled at\n\
initiation; its per-step outcomes are the goal.step events counted in\n\
section 4. skill.run is journaled at completion; its start is derived from\n\
the recorded duration.\n\n",
        );
    }

    // ---------- 6. integrity ----------
    s.push_str(&format!(
        "## 6. Integrity & tamper evidence\n\n\
- chain construction: each journal line embeds `prev` (SHA-256 of the\n\
  previous line) and `hash` (SHA-256 of the line itself with the hash member\n\
  removed). Editing, deleting or reordering any line breaks every later link.\n\
- journal HEAD at generation: {head}\n\
- verification result: chain intact, {events} events. This annex refuses to\n\
  generate from a broken chain, so its own existence asserts the result.\n",
    ));
    if col.anchors.is_empty() {
        s.push_str(
            "- external anchors: none yet — run `soma anchor now`. Without an anchor the\n\
  operator could regenerate and re-sign the entire chain; an RFC 3161 anchor\n\
  pins the head at a third party's clock.\n",
        );
    } else {
        s.push_str(
            "- external anchors (RFC 3161 timestamps over the head hash, from the\n\
  journal's granted journal.anchor records):\n",
        );
        for a in &col.anchors {
            s.push_str(&format!(
                "    - seq {}: head {} via {} (.soma/anchors/{})\n",
                a.i_of("seq"),
                truncate_chars(&a.str_of("head"), 16),
                a.str_of("url"),
                a.str_of("tsr_file"),
            ));
        }
    }
    s.push_str(
        "\nWhat tampering would NOT be detectable:\n\n\
- truncation-before-anchor windows: chain verification proves internal\n\
  consistency of what is present. Events appended AFTER the newest anchor\n\
  could be truncated (together with the head file) without detection; only\n\
  the prefix up to each anchored seq is pinned by a third party. A full\n\
  regenerate-and-re-sign of the chain is detectable only where anchors exist.\n\
- events never journaled leave no trace (caveat C5; section 10).\n\n",
    );

    // ---------- 7. retention ----------
    s.push_str(&format!(
        "## 7. Retention (Articles 19(1) and 26(6))\n\n\
Article 19(1) (providers) and Article 26(6) (deployers) use the same\n\
formula: logs are kept \"to the extent such logs are under their control\",\n\
\"for a period appropriate to the intended purpose of the high-risk AI\n\
system, of at least six months, unless provided otherwise in the applicable\n\
Union or national law, in particular in Union law on the protection of\n\
personal data.\"\n\n\
- six months is a floor, not a ceiling; \"appropriate to the intended\n\
  purpose\" can require materially longer.\n\
- GDPR storage limitation (Article 5(1)(e) GDPR) can compel earlier erasure\n\
  of personal data inside logs; the redaction-before-write design (section 3)\n\
  minimises personal data on the chain, but the operator remains responsible\n\
  (caveat C6).\n\
- financial institutions keep the logs under their financial-services\n\
  documentation regime instead (Articles 19(2), 26(6) second subparagraph).\n\n\
Actual state of this journal:\n\n\
- first event: {first} — current age {age_days} days (the six-month floor is\n\
  ~183 days)\n\
- last event: {last}\n\
- soma never deletes journal events; retention here is bounded only by the\n\
  operator and the filesystem. Since the logs are under the deployer's\n\
  control (section 3), Article 26(6) is the operative duty.\n\n{dates}\n",
        first = col.first_iso,
        last = col.last_iso,
    ));

    // ---------- 8. access & production ----------
    s.push_str(
        "## 8. Access & production (Article 21(2))\n\n\
Article 21(2): upon reasoned request, providers give competent authorities\n\
\"access to the automatically generated logs of the high-risk AI system\n\
referred to in Article 12(1), to the extent such logs are under their\n\
control\" (mirrored for authorised representatives in Article 22(3)(c)).\n\
Production paths, in increasing portability:\n\n\
- live inspection: `soma log tail [-n N]`, `soma log show <id>`,\n\
  `soma log verify`\n\
- portable evidence bundle: `soma export` — full journal + state snapshots +\n\
  anchors + manifest + VERIFY.md; verifiable on any machine with shasum\n\
  alone, no soma required\n\
- this annex: `soma export eu-ai-act` (markdown + JSON sibling)\n\
- raw: .soma/events.jsonl is plain JSONL — readable without any tooling\n\n",
    );

    // ---------- 9. auditor verification ----------
    s.push_str(&format!(
        "## 9. Auditor verification instructions\n\n\
Reproducible on any machine; expected outputs for a pristine journal noted.\n\n\
1. Chain: `soma log verify` must report ok with head {head_short}\n\
   Without soma: for each line of events.jsonl, `hash` is the SHA-256 of the\n\
   line with the `,\"hash\":\"…\"` member removed, and `prev` equals the\n\
   previous line's `hash` (first line: \"genesis\").\n\
2. Bundle: `soma export`, then `soma export verify <dir>` must print\n\
   \"bundle OK\". Without soma: `shasum -a 256 <file>` must equal the\n\
   `sha256` of every entry under `files` in the bundle's manifest.json.\n\
3. Anchors (when present): the anchored message is the 64-char head hash as\n\
   ASCII; its imprint is `printf '<head>' | shasum -a 256`. For each granted\n\
   anchor in section 6, with the TSA root CA fetched per the bundle's\n\
   VERIFY.md:\n\n",
        head_short = truncate_chars(head, 12),
    ));
    if col.anchors.is_empty() {
        s.push_str("   (no granted anchors on this chain yet — `soma anchor now`)\n\n");
    } else {
        s.push_str("```\n");
        for a in &col.anchors {
            let ahead = a.str_of("head");
            let imprint = crate::sha256::sha256_hex(ahead.as_bytes());
            s.push_str(&format!(
                "# anchor seq {} (head {})\n\
openssl ts -reply -in .soma/anchors/{tsr} -text\n\
openssl ts -verify -digest {imprint} -in .soma/anchors/{tsr} -CAfile <root.pem>\n\
openssl ts -verify -queryfile .soma/anchors/{tsq} -in .soma/anchors/{tsr} -CAfile <root.pem>\n",
                a.i_of("seq"),
                truncate_chars(&ahead, 16),
                tsr = a.str_of("tsr_file"),
                tsq = a.str_of("tsq_file"),
            ));
        }
        s.push_str(
            "```\n\n\
   Expect `Verification: OK`. A wrong digest MUST fail with\n\
   `message imprint mismatch` — try it as a negative control.\n\n",
        );
    }
    s.push_str(
        "4. This annex: regenerate with `soma export eu-ai-act` and compare the\n\
   kinds histogram and head against the JSON sibling (the head will advance\n\
   by the export.bundle events the generations themselves append).\n\n",
    );

    // ---------- 10. gaps ----------
    s.push_str(
        "## 10. Coverage, gaps & limitations\n\n\
What is NOT logged (caveat C5):\n\n\
- model internals: prompts/outputs are journaled only as routed through soma\n\
  (model.call / model.ask); provider-side internals — reasoning traces,\n\
  training data, upstream API internals — are out of reach.\n\
- child-process activity beyond captured streams: `soma wrap` journals the\n\
  launch, the exit, and SHA-256 + bounded excerpts of stdout/stderr; it does\n\
  not syscall- or network-sandbox the child. File and network activity of a\n\
  wrapped agent that bypasses stdout/stderr leaves no trace here.\n\
- events outside the wrapped/instrumented boundary: anything an operator or\n\
  agent does without going through soma is invisible to this journal.\n\
- full stream contents: wrapped output is recorded as hashes plus 2 KiB\n\
  head/tail excerpts (post-redaction), not in full.\n\
- clock source: timestamps come from the local system clock. Only RFC 3161\n\
  anchors (section 6) bind any part of the chain to an external clock, and\n\
  only as an upper bound (\"existed no later than\").\n\n",
    );

    // ---------- appendices ----------
    s.push_str(&format!(
        "## Appendix A — Journal statistics\n\n\
- events: {events}\n\
- distinct kinds: {kinds}\n\
- window: {first} → {last}\n\
- journal HEAD: {head}\n\
- policy refusals journaled: {denials}\n\
- granted anchors: {anchors}\n\n",
        kinds = col.kinds.len(),
        first = col.first_iso,
        last = col.last_iso,
        denials = col.denials,
        anchors = col.anchors.len(),
    ));
    s.push_str(
        "## Appendix B — Cross-reference (annex section → AI Act provision)\n\n\
| section | provision |\n\
|---|---|\n\
| 0 | document control; caveat C7 → Article 113 + AI Omnibus (provisional) |\n\
| 1 | Article 11 / Annex IV(1) general description; Articles 3(3), 3(4), 3(11) |\n\
| 2 | Article 6, Annex III; Articles 6(3), 6(4), 49(2) |\n\
| 3 | Article 12(1); Articles 13(3)(f), 16(e), 19(1), 26(6) (control) |\n\
| 4 | Article 12(2)(a)–(c); Articles 79(1), 72, 26(5) |\n\
| 5 | Article 12(3) (stated not applicable — Annex III 1(a) only) |\n\
| 6 | supports the reliability of Article 12(1) records (no dedicated article) |\n\
| 7 | Articles 19(1), 26(6); Article 5(1)(e) GDPR |\n\
| 8 | Article 21(2); Article 22(3)(c) |\n\
| 9 | verification procedure (no dedicated article) |\n\
| 10 | honest limits (caveat C5; no dedicated article) |\n\n\
(Appendix C — glossary — is deliberately omitted; terms are defined where\n\
they are used.)\n\n",
    );
    s.push_str(&format!(
        "## Appendix D — Generation manifest\n\n\
- tool: soma {SOMA_VERSION}\n\
- format: eu-ai-act, version 1\n\
- generated at: {generated_at}\n\
- legal-status snapshot: {LEGAL_SNAPSHOT}\n\
- journal digest (HEAD): {head}\n\
- events: {events}\n\
- operator config used: aiact.system={system} · aiact.provider={provider} ·\n\
  aiact.deployer={deployer} · aiact.intended_purpose={purpose} ·\n\
  aiact.classification={classification}\n\
- outputs: {md_name} + {json_name}\n"
    ));
    s
}

fn render_json(
    c: &Ctx,
    report: &crate::events::VerifyReport,
    col: &Collected,
    generated_at: &str,
    age_days: i64,
    fields: [&String; 5],
) -> Json {
    let [system, provider, deployer, purpose, classification] = fields;
    let kinds: Vec<Json> = col
        .kinds
        .iter()
        .map(|(kind, st)| {
            let (a, b, cc) = art12_map(kind);
            jobj(vec![
                ("kind", jstr(kind)),
                ("count", jint(st.count)),
                ("first", jstr(&st.first)),
                ("last", jstr(&st.last)),
                ("art12_2a", jbool(a)),
                ("art12_2b", jbool(b)),
                ("art12_2c", jbool(cc)),
            ])
        })
        .collect();
    let periods: Vec<Json> = col
        .periods
        .iter()
        .map(|p| {
            jobj(vec![
                ("source", jstr(p.source)),
                ("label", jstr(&p.label)),
                ("start", jstr(&p.start)),
                (
                    "end",
                    p.end.as_ref().map(|e| jstr(e)).unwrap_or(Json::Null),
                ),
                (
                    "duration_ms",
                    p.duration_ms.map(jint).unwrap_or(Json::Null),
                ),
                ("exit", p.exit.map(jint).unwrap_or(Json::Null)),
                ("ok", p.ok.map(jbool).unwrap_or(Json::Null)),
                ("timed_out", jbool(p.timed_out)),
            ])
        })
        .collect();
    let anchors: Vec<Json> = col
        .anchors
        .iter()
        .map(|a| {
            jobj(vec![
                ("seq", jint(a.i_of("seq"))),
                ("head", jstr(&a.str_of("head"))),
                ("url", jstr(&a.str_of("url"))),
                ("tsr_file", jstr(&a.str_of("tsr_file"))),
                ("status", jstr(&a.str_of("status"))),
            ])
        })
        .collect();
    jobj(vec![
        (
            "generator",
            jobj(vec![
                ("name", jstr("soma")),
                ("version", jstr(SOMA_VERSION)),
                ("format", jstr("eu-ai-act")),
                ("format_version", jint(1)),
            ]),
        ),
        ("generated_at", jstr(generated_at)),
        ("legal_status_snapshot", jstr(LEGAL_SNAPSHOT)),
        (
            "regulation",
            jobj(vec![("celex", jstr(CELEX)), ("eli", jstr(ELI))]),
        ),
        (
            "application_dates",
            jobj(vec![
                (
                    "in_force",
                    jobj(vec![
                        ("annex_iii", jstr(IN_FORCE_ANNEX3)),
                        ("annex_i", jstr(IN_FORCE_ANNEX1)),
                    ]),
                ),
                (
                    "omnibus_provisional",
                    jobj(vec![
                        ("annex_iii", jstr(OMNIBUS_ANNEX3)),
                        ("annex_i", jstr(OMNIBUS_ANNEX1)),
                        ("status", jstr(OMNIBUS_STATUS)),
                    ]),
                ),
            ]),
        ),
        (
            "caveats",
            jarr(CAVEAT_CODES.iter().map(|c| jstr(*c)).collect()),
        ),
        (
            "system",
            jobj(vec![
                ("project", jstr(&c.name())),
                ("system", jstr(system)),
                ("provider", jstr(provider)),
                ("deployer", jstr(deployer)),
                ("intended_purpose", jstr(purpose)),
                ("classification", jstr(classification)),
            ]),
        ),
        (
            "journal",
            jobj(vec![
                ("events", jint(report.events as i64)),
                ("head", jstr(&report.head)),
                ("verified", jbool(true)),
                ("first_event", jstr(&col.first_iso)),
                ("last_event", jstr(&col.last_iso)),
                ("age_days", jint(age_days)),
            ]),
        ),
        ("kinds", jarr(kinds)),
        ("periods_of_use", jarr(periods)),
        ("anchors", jarr(anchors)),
        (
            "retention",
            jobj(vec![
                ("floor_months", jint(6)),
                ("journal_age_days", jint(age_days)),
                ("journal_first_event", jstr(&col.first_iso)),
                ("soma_deletes_logs", jbool(false)),
            ]),
        ),
        (
            "policy",
            jobj(vec![
                ("autonomy", jstr(&c.policy.autonomy)),
                ("denials", jint(col.denials)),
            ]),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::jobj as o;
    use crate::project::testutil::temp_ctx;

    fn gen(c: &Ctx) -> (String, Json, PathBuf, PathBuf) {
        let (md_path, json_path) = export_aiact(c, None).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        let j = crate::json::parse(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        (md, j, md_path, json_path)
    }

    #[test]
    fn refuses_broken_chain() {
        let (base, c) = temp_ctx();
        c.log("a", o(vec![])).unwrap();
        c.log("b", o(vec![])).unwrap();
        let jp = c.dir.join("events.jsonl");
        let content = std::fs::read_to_string(&jp).unwrap().replace("\"a\"", "\"x\"");
        std::fs::write(&jp, content).unwrap();
        let err = export_aiact(&c, None).unwrap_err();
        assert!(err.contains("refusing to export"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn caveats_on_page_one_and_both_dates_present() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (md, j, _, _) = gen(&c);
        // All eight caveats present, and the block sits BEFORE section 1.
        for code in CAVEAT_CODES {
            assert!(md.contains(&format!("- {code} —")), "missing caveat {code}");
        }
        let last_caveat = md.find("- C8 —").unwrap();
        let section_1 = md.find("\n## 1.").unwrap();
        assert!(last_caveat < section_1, "caveats must be on page 1, before section 1");
        assert!(md.contains("conformity assessment under Article 43"));
        assert!(md.contains("not an Annex III area"));
        // BOTH date sets, everywhere dates appear, with the snapshot stamp.
        for d in ["2026-08-02", "2027-08-02", "2027-12-02", "2028-08-02", LEGAL_SNAPSHOT] {
            assert!(md.contains(d), "missing date {d}");
        }
        let ad = j.get("application_dates").expect("application_dates");
        assert_eq!(ad.get("in_force").unwrap().str_of("annex_iii"), "2026-08-02");
        assert_eq!(ad.get("omnibus_provisional").unwrap().str_of("annex_iii"), "2027-12-02");
        assert_eq!(ad.get("omnibus_provisional").unwrap().str_of("annex_i"), "2028-08-02");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn missing_operator_fields_render_placeholder() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (md, j, _, _) = gen(&c);
        assert!(md.contains(PLACEHOLDER), "placeholder must be explicit");
        assert_eq!(j.get("system").unwrap().str_of("provider"), PLACEHOLDER);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn operator_fields_render_when_configured() {
        let (base, mut c) = temp_ctx();
        c.config.set(
            "aiact",
            o(vec![
                ("provider", jstr("ACME GmbH")),
                ("classification", jstr("out-of-scope")),
            ]),
        );
        c.log("test.event", o(vec![])).unwrap();
        let (md, j, _, _) = gen(&c);
        assert!(md.contains("ACME GmbH"));
        assert!(md.contains("outside Annex III"), "enum rendered as prose");
        assert_eq!(j.get("system").unwrap().str_of("provider"), "ACME GmbH");
        assert_eq!(j.get("system").unwrap().str_of("classification"), "out-of-scope");
        // deployer was not provided → still the placeholder, never omitted.
        assert_eq!(j.get("system").unwrap().str_of("deployer"), PLACEHOLDER);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn period_of_use_from_wrap_pair_and_skill_run() {
        let (base, c) = temp_ctx();
        // Synthetic wrap.start/wrap.end pair — D9 field names verbatim.
        c.log(
            "wrap.start",
            o(vec![
                ("label", jstr("claude")),
                ("cmd", jstr("claude")),
                ("args", jarr(vec![jstr("-p"), jstr("task")])),
                ("cwd", jstr("/tmp")),
                ("pid", jint(4242)),
            ]),
        )
        .unwrap();
        c.log(
            "wrap.end",
            o(vec![
                ("label", jstr("claude")),
                ("exit", jint(0)),
                ("duration_ms", jint(1500)),
                ("timed_out", jbool(false)),
            ]),
        )
        .unwrap();
        // An unmatched start (crash) must still appear, end = —.
        c.log(
            "wrap.start",
            o(vec![("label", jstr("crashed")), ("pid", jint(4243))]),
        )
        .unwrap();
        c.log(
            "skill.run",
            o(vec![
                ("name", jstr("greet")),
                ("ok", jbool(true)),
                ("ms", jint(40)),
                ("detail", jstr("hello")),
            ]),
        )
        .unwrap();
        let (md, j, _, _) = gen(&c);
        assert!(md.contains("| wrap | claude | exit 0 |"), "{md}");
        assert!(md.contains("1500 ms"));
        assert!(md.contains("| — | — | wrap | crashed | — |"), "unmatched start row: {md}");
        assert!(md.contains("| skill.run | greet | ok |"));
        let periods = j.get("periods_of_use").unwrap().arr().unwrap().clone();
        let wrap = periods
            .iter()
            .find(|p| p.str_of("source") == "wrap" && p.str_of("label") == "claude")
            .expect("wrap period row");
        assert_eq!(wrap.i_of("duration_ms"), 1500);
        assert_eq!(wrap.i_of("exit"), 0);
        assert!(!wrap.str_of("start").is_empty());
        assert!(!wrap.str_of("end").is_empty());
        let skill = periods
            .iter()
            .find(|p| p.str_of("source") == "skill.run")
            .expect("skill period row");
        assert!(skill.b_of("ok"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn concurrent_same_label_wraps_pair_by_pid_not_swapped() {
        // F9 — two same-label wraps run concurrently. Without (label, pid)
        // pairing, end(pid=1) would mis-attribute to the most recent pending
        // start (pid=2) and swap the START time. Pairing on pid fixes it.
        let (base, c) = temp_ctx();
        // start A (pid 1) FIRST, with an earlier timestamp.
        c.log(
            "wrap.start",
            o(vec![("label", jstr("agent")), ("pid", jint(1))]),
        )
        .unwrap();
        let start_a = c.journal().tail(1).unwrap()[0].str_of("iso");
        // Ensure a distinct millisecond iso for start B.
        std::thread::sleep(std::time::Duration::from_millis(5));
        // start B (pid 2) SECOND, later timestamp.
        c.log(
            "wrap.start",
            o(vec![("label", jstr("agent")), ("pid", jint(2))]),
        )
        .unwrap();
        let start_b = c.journal().tail(1).unwrap()[0].str_of("iso");
        assert_ne!(start_a, start_b, "the two starts need distinct isos for this test");

        // end for pid 1 completes FIRST (interleaved completion) — distinct
        // duration so we can tell the two period rows apart.
        c.log(
            "wrap.end",
            o(vec![
                ("label", jstr("agent")),
                ("pid", jint(1)),
                ("exit", jint(0)),
                ("duration_ms", jint(111)),
                ("timed_out", jbool(false)),
            ]),
        )
        .unwrap();
        c.log(
            "wrap.end",
            o(vec![
                ("label", jstr("agent")),
                ("pid", jint(2)),
                ("exit", jint(0)),
                ("duration_ms", jint(222)),
                ("timed_out", jbool(false)),
            ]),
        )
        .unwrap();

        let (_, j, _, _) = gen(&c);
        let periods = j.get("periods_of_use").unwrap().arr().unwrap().clone();
        let p1 = periods
            .iter()
            .find(|p| p.i_of("duration_ms") == 111)
            .expect("pid-1 period");
        let p2 = periods
            .iter()
            .find(|p| p.i_of("duration_ms") == 222)
            .expect("pid-2 period");
        // The pid-1 end must carry pid-1's (earlier) start time, NOT pid-2's.
        assert_eq!(p1.str_of("start"), start_a, "pid-1 end must keep pid-1's start");
        assert_eq!(p2.str_of("start"), start_b, "pid-2 end must keep pid-2's start");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn period_table_label_with_pipe_and_newline_stays_one_row() {
        // F7 — a label carrying `|` and a newline must NOT inject extra columns
        // or break the GFM table: it renders as a single intact row.
        let (base, c) = temp_ctx();
        c.log(
            "wrap.start",
            o(vec![
                ("label", jstr("evil | col\ninjected")),
                ("pid", jint(7777)),
            ]),
        )
        .unwrap();
        c.log(
            "wrap.end",
            o(vec![
                ("label", jstr("evil | col\ninjected")),
                ("exit", jint(0)),
                ("duration_ms", jint(10)),
                ("pid", jint(7777)),
                ("timed_out", jbool(false)),
            ]),
        )
        .unwrap();
        let (md, j, _, _) = gen(&c);
        // The data row is the line mentioning the wrap source after the period
        // table header. Find the row that contains the (escaped) label.
        let row = md
            .lines()
            .find(|l| l.contains("evil") && l.starts_with('|'))
            .expect("the period row for the evil label");
        // Pipe escaped, newline flattened to a space → single line.
        assert!(row.contains("evil \\| col injected"), "label must be escaped: {row}");
        assert!(!row.contains("\ninjected"), "newline must be flattened");
        // Exactly 6 columns: 7 UNESCAPED `|` delimiters. The label's pipe is
        // escaped as `\|` (a literal cell character in GFM, not a delimiter),
        // so it must not be counted — count delimiters by stripping `\|` first.
        let delims = row.replace("\\|", "").matches('|').count();
        assert_eq!(delims, 7, "row must have exactly 6 cells (7 delimiters): {row}");
        // The JSON sibling is unaffected — label stored verbatim (structured).
        let periods = j.get("periods_of_use").unwrap().arr().unwrap().clone();
        assert!(
            periods.iter().any(|p| p.str_of("label") == "evil | col\ninjected"),
            "json label must be the raw, unescaped value"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn histogram_counts_and_journaled_generation() {
        let (base, c) = temp_ctx();
        for _ in 0..3 {
            c.log("test.event", o(vec![])).unwrap();
        }
        let (md, j, _, _) = gen(&c);
        assert!(md.contains("| test.event | 3 |"), "{md}");
        let kinds = j.get("kinds").unwrap().arr().unwrap().clone();
        let te = kinds.iter().find(|k| k.str_of("kind") == "test.event").unwrap();
        assert_eq!(te.i_of("count"), 3);
        assert!(!te.str_of("first").is_empty() && !te.str_of("last").is_empty());
        // Generation journaled as export.bundle format:"eu-ai-act" (otlp precedent).
        let tail = c.journal().tail(3).unwrap();
        let ev = tail
            .iter()
            .find(|e| e.str_of("kind") == "export.bundle")
            .expect("export.bundle event");
        assert_eq!(ev.get("data").unwrap().str_of("format"), "eu-ai-act");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn json_sibling_parses_with_stable_keys() {
        let (base, c) = temp_ctx();
        c.log("test.event", o(vec![])).unwrap();
        let (_, j, md_path, json_path) = gen(&c);
        assert_eq!(json_path, md_path.with_extension("json"));
        assert_eq!(j.get("generator").unwrap().str_of("format"), "eu-ai-act");
        assert_eq!(j.get("generator").unwrap().str_of("version"), SOMA_VERSION);
        assert_eq!(j.get("caveats").unwrap().arr().unwrap().len(), 8);
        assert_eq!(j.str_of("legal_status_snapshot"), LEGAL_SNAPSHOT);
        assert_eq!(j.get("regulation").unwrap().str_of("celex"), CELEX);
        let journal = j.get("journal").unwrap();
        assert!(journal.b_of("verified"));
        assert!(!journal.str_of("head").is_empty());
        assert!(journal.i_of("events") >= 2);
        assert_eq!(j.get("retention").unwrap().i_of("floor_months"), 6);
        assert!(!j.get("retention").unwrap().b_of("soma_deletes_logs"));
        assert!(j.get("periods_of_use").unwrap().arr().is_some());
        assert!(j.get("anchors").unwrap().arr().unwrap().is_empty());
        assert!(!j.get("policy").unwrap().str_of("autonomy").is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn granted_anchor_listed_with_openssl_instructions() {
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
        // A failed attempt must NOT be listed as an anchor.
        c.log(
            "journal.anchor",
            o(vec![("seq", jint(99)), ("status", jstr("failed"))]),
        )
        .unwrap();
        let (md, j, _, _) = gen(&c);
        let imprint = crate::sha256::sha256_hex(rep.head.as_bytes());
        assert!(md.contains(&format!(
            "openssl ts -verify -digest {imprint} -in .soma/anchors/anchor-2-test.tsr"
        )));
        assert!(md.contains("-queryfile .soma/anchors/anchor-2-test.tsq"));
        let anchors = j.get("anchors").unwrap().arr().unwrap().clone();
        assert_eq!(anchors.len(), 1, "granted only");
        assert_eq!(anchors[0].str_of("status"), "granted");
        std::fs::remove_dir_all(&base).ok();
    }
}
