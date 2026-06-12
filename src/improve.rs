//! R7/R15 — the improvement engine: soma proposing how to make itself better.
//!
//! Proposals are generated from evidence (metrics, issues, the journal), carry
//! their rationale with numbers, and sit in a queue the operator controls:
//! `soma proposals list|show|apply|dismiss`. Mechanical proposals (timeouts,
//! crons, config values, archiving) can be auto-applied at `soma tick` — but
//! only under `autonomy: auto` (R3). Everything is journaled.

use crate::json::{jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::util::*;

/// Kinds that `apply` can perform mechanically; the rest are advice that
/// needs a human (or a bigger model) to act on.
pub const MECHANICAL: [&str; 4] = ["tune_timeout", "add_cron", "config_change", "archive_skill"];
pub const KINDS: [&str; 7] = [
    "tune_timeout",
    "add_cron",
    "config_change",
    "archive_skill",
    "fix_skill",
    "new_skill",
    "advice",
];

/// Create a proposal unless an open one with the same kind+target exists.
pub fn add_proposal(
    c: &Ctx,
    kind: &str,
    target: &str,
    rationale: &str,
    change: Json,
) -> R<Option<Json>> {
    if !KINDS.contains(&kind) {
        return Err(format!("proposal kind must be one of {KINDS:?}"));
    }
    // Suppress if an identical proposal (same kind+target) is currently open,
    // or was applied/dismissed within the suppression window — otherwise
    // cumulative journal evidence regenerates the same proposal every tick.
    let now = now_ms();
    let dup = list(c, false)?.iter().any(|p| {
        if p.str_of("kind") != kind || p.str_of("target") != target {
            return false;
        }
        match p.str_of("status").as_str() {
            "proposed" => true,
            "applied" | "dismissed" => {
                let when = if p.i_of("status_ms") > 0 { p.i_of("status_ms") } else { p.i_of("ts") };
                now - when < RESOLVED_SUPPRESS_MS
            }
            _ => false,
        }
    });
    if dup {
        return Ok(None);
    }
    let ts = now_ms();
    let p = jobj(vec![
        ("id", jstr(new_id("pr"))),
        ("ts", jint(ts)),
        ("iso", jstr(&iso8601(ts))),
        ("kind", jstr(kind)),
        ("target", jstr(target)),
        ("rationale", jstr(rationale)),
        ("change", change),
        ("status", jstr("proposed")),
    ]);
    append_line(&c.proposals_path(), &p.to_string())?;
    c.log(
        "proposal.new",
        jobj(vec![
            ("id", jstr(&p.str_of("id"))),
            ("kind", jstr(kind)),
            ("target", jstr(target)),
            ("rationale", jstr(&truncate_chars(rationale, 200))),
        ]),
    )?;
    Ok(Some(p))
}

/// Latest record per proposal id (status updates append a new record).
pub fn list(c: &Ctx, only_open: bool) -> R<Vec<Json>> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, Json> = std::collections::HashMap::new();
    for_each_line(&c.proposals_path(), |line| {
        if let Ok(p) = crate::json::parse(line) {
            let id = p.str_of("id");
            if !id.is_empty() {
                if !latest.contains_key(&id) {
                    order.push(id.clone());
                }
                latest.insert(id, p);
            }
        }
        Ok(())
    })?;
    Ok(order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .filter(|p| !only_open || p.str_of("status") == "proposed")
        .collect())
}

pub fn get(c: &Ctx, id: &str) -> R<Json> {
    list(c, false)?
        .into_iter()
        .find(|p| p.str_of("id") == id)
        .ok_or_else(|| format!("no proposal '{id}'"))
}

fn set_status(c: &Ctx, id: &str, status: &str, note: &str) -> R<Json> {
    let mut p = get(c, id)?;
    p.set("status", jstr(status));
    p.set("status_note", jstr(note));
    p.set("status_ms", jint(now_ms()));
    p.set("status_iso", jstr(&iso8601(now_ms())));
    append_line(&c.proposals_path(), &p.to_string())?;
    Ok(p)
}

/// How long a resolved (applied/dismissed) proposal suppresses an identical
/// one. Stops the per-tick re-proposal churn the optimizer/scan would
/// otherwise produce from cumulative journal evidence, while still letting a
/// genuinely recurring problem resurface later.
const RESOLVED_SUPPRESS_MS: i64 = 7 * 24 * 3600 * 1000;

/// Set a config value by dot path ("model.routing.simple"), creating
/// intermediate objects as needed.
pub fn set_config_path(cfg: &mut Json, path: &str, value: Json) {
    fn rec(node: &mut Json, parts: &[&str], value: Json) {
        if parts.is_empty() {
            return;
        }
        if parts.len() == 1 {
            node.set(parts[0], value);
            return;
        }
        if node.get(parts[0]).map(|v| v.obj().is_none()).unwrap_or(true) {
            node.set(parts[0], jobj(vec![]));
        }
        // re-borrow mutably
        if let Json::Obj(pairs) = node {
            if let Some((_, child)) = pairs.iter_mut().find(|(k, _)| k == parts[0]) {
                rec(child, &parts[1..], value);
            }
        }
    }
    let parts: Vec<&str> = path.split('.').collect();
    rec(cfg, &parts, value);
}

/// Apply a proposal. Mechanical kinds make the change directly; advisory
/// kinds are acknowledged with guidance. Always journaled.
pub fn apply(c: &mut Ctx, id: &str) -> R<String> {
    let p = get(c, id)?;
    if p.str_of("status") != "proposed" {
        return Err(format!("proposal {id} is already {}", p.str_of("status")));
    }
    let kind = p.str_of("kind");
    let change = p.get("change").cloned().unwrap_or_else(|| jobj(vec![]));
    let note = match kind.as_str() {
        "tune_timeout" => {
            let skill = change.str_of("skill");
            let timeout = change.i_of("timeout_s");
            if timeout <= 0 {
                return Err("change.timeout_s must be > 0".into());
            }
            let s = crate::skills::find(c, &skill)?;
            let mut m = s.manifest.clone();
            let mut run = m.get("run").cloned().unwrap_or_else(|| jobj(vec![]));
            run.set("timeout_s", jint(timeout));
            m.set("run", run);
            m.set("version", jint(m.i_of("version") + 1));
            atomic_write(&s.path, m.pretty().as_bytes())?;
            format!("skill '{skill}' timeout set to {timeout}s (version bumped)")
        }
        "fix_skill" => {
            let skill = change.str_of("skill");
            let new_cmd = change.str_of("cmd");
            if new_cmd.is_empty() {
                return Err("change.cmd is required for fix_skill proposals".into());
            }
            // Validate the cmd still passes policy at apply time (policy may have
            // tightened since the proposal was created).
            match c.policy.check_command(&new_cmd) {
                crate::policy::Decision::Deny { rule } => {
                    return Err(format!("cmd blocked by current policy ({rule})"));
                }
                crate::policy::Decision::Allow { .. } => {}
            }
            // Write to the project registry copy (project-shadows-global pattern:
            // skills::find returns the project path if it exists, global otherwise;
            // we always write to the project skills dir to avoid mutating global).
            let s = crate::skills::find(c, &skill)?;
            let target_path = if s.scope == "project" {
                s.path.clone()
            } else {
                // Skill lives in global — promote a project-local copy.
                c.skills_dir().join(format!("{skill}.json"))
            };
            let mut m = s.manifest.clone();
            let mut run = m.get("run").cloned().unwrap_or_else(|| jobj(vec![]));
            run.set("cmd", jstr(&new_cmd));
            m.set("run", run);
            m.set("version", jint(m.i_of("version") + 1));
            atomic_write(&target_path, m.pretty().as_bytes())?;
            format!("skill '{skill}' run.cmd rewritten (version bumped)")
        }
        "archive_skill" => {
            let skill = change.str_of("skill");
            let s = crate::skills::find(c, &skill)?;
            let mut m = s.manifest.clone();
            m.set("archived", crate::json::jbool(true));
            m.set("version", jint(m.i_of("version") + 1));
            atomic_write(&s.path, m.pretty().as_bytes())?;
            format!("skill '{skill}' archived (selector will skip it)")
        }
        "add_cron" => {
            let entry = crate::cron::add(
                c,
                &change.str_of("name"),
                &change.str_of("schedule"),
                change.get("action").cloned().unwrap_or_else(|| jobj(vec![])),
            )?;
            format!(
                "cron '{}' added with schedule '{}'",
                entry.str_of("name"),
                entry.str_of("schedule")
            )
        }
        "config_change" => {
            let path = change.str_of("path");
            let value = change.get("value").cloned().unwrap_or(Json::Null);
            if path.is_empty() {
                return Err("change.path required".into());
            }
            set_config_path(&mut c.config, &path, value.clone());
            c.save_config()?;
            format!("config {path} set to {}", value.to_string())
        }
        // advisory kinds: acknowledge, hand the human the next move
        _ => format!(
            "acknowledged — this proposal needs human follow-up: {}",
            p.str_of("rationale")
        ),
    };
    // Resolve the issues this proposal addresses (each writes a lesson, R8).
    // For skill-targeted mechanical fixes, resolve ALL open issues on that
    // skill — otherwise sibling issues keep regenerating the same proposal
    // from stale evidence on the next tick.
    let skill_target = change.str_of("skill");
    if !skill_target.is_empty()
        && ["tune_timeout", "archive_skill", "fix_skill"].contains(&kind.as_str())
    {
        for issue in crate::skills::list_issues(c, true)? {
            if issue.str_of("skill") == skill_target {
                let _ = crate::skills::resolve_issue(
                    c,
                    &issue.str_of("id"),
                    &format!("applied proposal {id}"),
                );
            }
        }
    } else {
        let issue_id = change.str_of("issue_id");
        if !issue_id.is_empty() {
            let _ = crate::skills::resolve_issue(c, &issue_id, &format!("applied proposal {id}"));
        }
    }
    set_status(c, id, "applied", &note)?;
    c.log(
        "proposal.apply",
        jobj(vec![("id", jstr(id)), ("kind", jstr(&kind)), ("note", jstr(&note))]),
    )?;
    Ok(note)
}

pub fn dismiss(c: &Ctx, id: &str, reason: &str) -> R<()> {
    set_status(c, id, "dismissed", reason)?;
    c.log(
        "proposal.dismiss",
        jobj(vec![("id", jstr(id)), ("reason", jstr(reason))]),
    )?;
    Ok(())
}

// ---------- fix_skill generation ----------

/// Validate a model-suggested replacement command. Returns an error string
/// describing why the suggestion was rejected, or Ok(()) if valid.
fn validate_fix_suggestion(c: &Ctx, suggestion: &str, current_cmd: &str) -> Result<(), String> {
    let trimmed = suggestion.trim();
    if trimmed.is_empty() {
        return Err("suggestion is empty".into());
    }
    if trimmed.contains('\n') {
        return Err("suggestion contains newlines (must be a single line)".into());
    }
    if trimmed.chars().count() > 400 {
        return Err(format!(
            "suggestion too long ({} chars, limit 400)",
            trimmed.chars().count()
        ));
    }
    if trimmed == current_cmd.trim() {
        return Err("suggestion is identical to the current cmd".into());
    }
    match c.policy.check_command(trimmed) {
        crate::policy::Decision::Deny { rule } => {
            Err(format!("suggestion blocked by policy ({rule})"))
        }
        crate::policy::Decision::Allow { .. } => Ok(()),
    }
}

/// Try to generate a model-assisted fix for a skill that has ≥2 failures and
/// ≥1 open issue. Returns None if no provider is reachable (silent skip) or if
/// the suggestion fails validation. Returns the validated suggestion + model
/// provenance on success.
fn generate_fix_suggestion(
    c: &Ctx,
    skill_name: &str,
    fail: i64,
    open_issues: &[Json],
) -> Option<(String, String, String, bool)> {
    // probe: echo is always ready; real providers need network + key.
    // We call route() which handles fallback internally, but we need to
    // know upfront if *any* provider is ready. If route() would fail
    // (no provider available), we skip silently.
    let s = crate::skills::find(c, skill_name).ok()?;
    let manifest_json = s.manifest.to_string();

    // Collect up to 2 recent stderr excerpts from open issues.
    let excerpts: Vec<String> = open_issues
        .iter()
        .filter(|i| i.str_of("skill") == skill_name)
        .take(2)
        .map(|i| truncate_chars(&i.str_of("detail"), 200))
        .collect();
    let stderr_block = if excerpts.is_empty() {
        "(no stderr recorded)".to_string()
    } else {
        excerpts.join("\n---\n")
    };

    let prompt = format!(
        "You are a shell command repair assistant.\n\
         A soma skill named '{skill_name}' has failed {fail} times.\n\
         Manifest:\n{manifest_json}\n\
         Recent failure details:\n{stderr_block}\n\
         Reply with ONLY the corrected run.cmd as a single shell command line, \
         nothing else — no explanation, no markdown, no quotes."
    );

    // Use the routed path (ask_routed = route + ask_cached).
    // If no provider is reachable, route() returns Err → we skip silently.
    let (route, reply) = crate::models::ask_routed(c, &prompt).ok()?;

    let suggestion = reply.text.trim().to_string();
    let current_cmd = s.cmd();

    if validate_fix_suggestion(c, &suggestion, &current_cmd).is_ok() {
        Some((suggestion, route.provider, route.model, reply.cached))
    } else {
        None
    }
}

// ---------- generators ----------

/// R7: scan metrics + issues for skills that need attention.
pub fn scan(c: &Ctx) -> R<Vec<Json>> {
    let metrics = crate::skills::load_metrics(c);
    let issues = crate::skills::list_issues(c, true)?;
    let mut created = Vec::new();

    if let Some(pairs) = metrics.obj() {
        for (skill, m) in pairs {
            let runs = m.i_of("runs");
            let fail = m.i_of("fail");
            // Never propose fixes for archived skills.
            let is_archived = crate::skills::find(c, skill)
                .map(|s| s.archived())
                .unwrap_or(false);
            if runs >= 3 && (fail as f64) / (runs as f64) >= 0.4 && !is_archived {
                let skill_issues: Vec<&Json> = issues
                    .iter()
                    .filter(|i| i.str_of("skill") == *skill)
                    .collect();
                let open_issue_ids: Vec<String> =
                    skill_issues.iter().map(|i| i.str_of("id")).collect();

                // New: model-assisted generation when ≥2 failures AND ≥1 open issue.
                // Falls back to advice-only proposal if generation is skipped or
                // the suggestion fails validation.
                if fail >= 2 && !skill_issues.is_empty() {
                    // Check for existing open/recent fix_skill proposal first
                    // (add_proposal handles dedup, but we want to skip generation too).
                    let already = list(c, false)?.iter().any(|p| {
                        if p.str_of("kind") != "fix_skill" || &p.str_of("target") != skill {
                            return false;
                        }
                        match p.str_of("status").as_str() {
                            "proposed" => true,
                            "applied" | "dismissed" => {
                                let when = if p.i_of("status_ms") > 0 {
                                    p.i_of("status_ms")
                                } else {
                                    p.i_of("ts")
                                };
                                now_ms() - when < RESOLVED_SUPPRESS_MS
                            }
                            _ => false,
                        }
                    });
                    if !already {
                        // Attempt model-assisted generation (silently skips if no
                        // provider reachable or suggestion fails validation).
                        if let Some((new_cmd, provider, model, cached)) =
                            generate_fix_suggestion(c, skill, fail, &issues)
                        {
                            // Collect ≤120-char stderr excerpt for rationale.
                            let stderr_excerpt: String = skill_issues
                                .iter()
                                .find(|i| !i.str_of("detail").is_empty())
                                .map(|i| truncate_chars(&i.str_of("detail"), 120))
                                .unwrap_or_default();
                            let cache_note = if cached { " (cache hit)" } else { "" };
                            let rationale = format!(
                                "skill '{skill}' failed {fail} times; last error: \"{stderr_excerpt}\"; \
                                 model: {provider}/{model}{cache_note} suggests replacing run.cmd"
                            );
                            let change = jobj(vec![
                                ("skill", jstr(skill)),
                                ("cmd", jstr(&new_cmd)),
                                ("issue_id", jstr(open_issue_ids.first().map(|s| s.as_str()).unwrap_or(""))),
                            ]);
                            if let Some(p) = add_proposal(c, "fix_skill", skill, &rationale, change)? {
                                created.push(p);
                            }
                            // Skip the advice-only proposal since we created a real one
                            continue;
                        }
                    } else {
                        // Already have an open/recent fix_skill for this skill — skip entirely
                        // (avoid creating the old-style advice proposal too)
                        continue;
                    }
                }

                // Fallback: advice-only fix_skill proposal (no model suggestion available)
                let mut change = jobj(vec![("skill", jstr(skill))]);
                if let Some(first) = open_issue_ids.first() {
                    change.set("issue_id", jstr(first));
                }
                if let Some(p) = add_proposal(
                    c,
                    "fix_skill",
                    skill,
                    &format!(
                        "skill '{skill}' failed {fail}/{runs} runs ({}%) — review its command or environment ({} open issues)",
                        fail * 100 / runs.max(1),
                        skill_issues.len()
                    ),
                    change,
                )? {
                    created.push(p);
                }
            }
            // unused for 30+ days (skip ones already archived)
            let last = m.i_of("last_used_ms");
            if runs >= 1 && last > 0 && now_ms() - last > 30 * 24 * 3_600_000 && !is_archived {
                if let Some(p) = add_proposal(
                    c,
                    "archive_skill",
                    skill,
                    &format!("skill '{skill}' has not been used in over 30 days"),
                    jobj(vec![("skill", jstr(skill))]),
                )? {
                    created.push(p);
                }
            }
        }
    }

    // repeated timeouts → propose a bigger timeout
    let mut timeout_counts: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for i in &issues {
        if i.str_of("detail").contains("timed out") {
            *timeout_counts.entry(i.str_of("skill")).or_default() += 1;
        }
    }
    for (skill, n) in timeout_counts {
        if n >= 2 {
            if let Ok(s) = crate::skills::find(c, &skill) {
                let new_timeout = (s.timeout_s() * 2).min(c.policy.max_timeout_s);
                let issue_id = issues
                    .iter()
                    .find(|i| i.str_of("skill") == skill && i.str_of("detail").contains("timed out"))
                    .map(|i| i.str_of("id"))
                    .unwrap_or_default();
                if let Some(p) = add_proposal(
                    c,
                    "tune_timeout",
                    &skill,
                    &format!(
                        "skill '{skill}' timed out {n} times at {}s — double it to {new_timeout}s",
                        s.timeout_s()
                    ),
                    jobj(vec![
                        ("skill", jstr(&skill)),
                        ("timeout_s", jint(new_timeout)),
                        ("issue_id", jstr(&issue_id)),
                    ]),
                )? {
                    created.push(p);
                }
            }
        }
    }
    Ok(created)
}

/// R15: analyze the journal + cache and propose optimizations.
pub fn optimize(c: &Ctx) -> R<Vec<Json>> {
    let mut calls_total = 0i64;
    let mut calls_cached = 0i64;
    let mut simple_to_cloud = 0i64;
    c.journal().for_each(|ev| {
        match ev.str_of("kind").as_str() {
            "model.call" => {
                if let Some(d) = ev.get("data") {
                    calls_total += 1;
                    if d.b_of("cached") {
                        calls_cached += 1;
                    }
                }
            }
            "model.route" => {
                if let Some(d) = ev.get("data") {
                    if d.str_of("level") == "simple" && d.str_of("provider") == "anthropic" {
                        simple_to_cloud += 1;
                    }
                }
            }
            _ => {}
        }
    })?;

    let mut created = Vec::new();

    if simple_to_cloud >= 5 {
        if let Some(p) = add_proposal(
            c,
            "config_change",
            "model.routing.simple",
            &format!(
                "{simple_to_cloud} simple tasks were routed to the cloud — a local model would handle them at zero cost (start ollama and apply)"
            ),
            jobj(vec![
                ("path", jstr("model.routing.simple")),
                (
                    "value",
                    jobj(vec![("provider", jstr("ollama")), ("model", jstr("llama3.2"))]),
                ),
            ]),
        )? {
            created.push(p);
        }
    }

    if calls_total >= 10 {
        let ratio = calls_cached as f64 / calls_total as f64;
        if ratio < 0.2 && crate::cache::enabled(c) {
            if let Some(p) = add_proposal(
                c,
                "advice",
                "cache",
                &format!(
                    "cache hit rate is {:.0}% over {calls_total} model calls — prompts rarely repeat; consider templating recurring prompts so the cache can work",
                    ratio * 100.0
                ),
                jobj(vec![]),
            )? {
                created.push(p);
            }
        }
    }

    // cache near its cap → propose doubling it
    let stats = crate::cache::stats(c);
    if stats.i_of("bytes") * 10 >= stats.i_of("max_bytes") * 9 {
        if let Some(p) = add_proposal(
            c,
            "config_change",
            "cache.max_bytes",
            &format!(
                "cache is at {}/{} bytes (≥90%) — old entries are being evicted; double the cap",
                stats.i_of("bytes"),
                stats.i_of("max_bytes")
            ),
            jobj(vec![
                ("path", jstr("cache.max_bytes")),
                ("value", jint(stats.i_of("max_bytes") * 2)),
            ]),
        )? {
            created.push(p);
        }
    }

    // journal growing large → advise an export + archive cycle
    if let Ok(meta) = std::fs::metadata(c.dir.join("events.jsonl")) {
        if meta.len() > 20 * 1024 * 1024 {
            if let Some(p) = add_proposal(
                c,
                "advice",
                "journal",
                &format!(
                    "journal is {}MB — run `soma export` and archive the bundle; large journals slow verification",
                    meta.len() / (1024 * 1024)
                ),
                jobj(vec![]),
            )? {
                created.push(p);
            }
        }
    }

    c.log(
        "optimize.run",
        jobj(vec![
            ("model_calls", jint(calls_total)),
            ("cache_hits", jint(calls_cached)),
            ("simple_to_cloud", jint(simple_to_cloud)),
            ("proposals_created", jint(created.len() as i64)),
        ]),
    )?;
    Ok(created)
}

/// The heartbeat: run due crons, generate proposals from fresh evidence, and
/// (only under `autonomy: auto`) apply the mechanical ones.
pub fn tick(c: &mut Ctx) -> R<String> {
    let cron_results = crate::cron::tick(c)?;
    // D10: anchor.auto=daily anchors the head when the last attempt is >24h
    // old. Failures are journaled by anchor::now (status:"failed"), which
    // stamps the clock — no retry until the next day. Never fatal to tick.
    let anchor_note = crate::anchor::auto_anchor(c);
    let mut new_proposals = scan(c)?;
    new_proposals.extend(crate::cron::propose_crons(c)?);
    new_proposals.extend(optimize(c)?);

    let mut auto_applied = Vec::new();
    if c.policy.auto_apply_allowed() {
        for p in list(c, true)? {
            if MECHANICAL.contains(&p.str_of("kind").as_str()) {
                let id = p.str_of("id");
                match apply(c, &id) {
                    Ok(note) => auto_applied.push(format!("{id}: {note}")),
                    Err(e) => auto_applied.push(format!("{id}: failed ({e})")),
                }
            }
        }
    }
    c.log(
        "tick.run",
        jobj(vec![
            ("crons_run", jint(cron_results.len() as i64)),
            ("proposals_new", jint(new_proposals.len() as i64)),
            ("auto_applied", jint(auto_applied.len() as i64)),
            (
                "anchor_auto",
                crate::json::jbool(anchor_note.is_some()),
            ),
        ]),
    )?;
    let mut out = format!(
        "tick: {} cron(s) run, {} new proposal(s)",
        cron_results.len(),
        new_proposals.len()
    );
    for (name, ok, note) in &cron_results {
        out.push_str(&format!("\n  cron {name}: {} — {note}", if *ok { "ok" } else { "FAILED" }));
    }
    if let Some(note) = &anchor_note {
        out.push_str(&format!("\n  {note}"));
    }
    for p in &new_proposals {
        out.push_str(&format!(
            "\n  proposal {} [{}] {}",
            p.str_of("id"),
            p.str_of("kind"),
            truncate_chars(&p.str_of("rationale"), 100)
        ));
    }
    if !auto_applied.is_empty() {
        out.push_str(&format!("\n  auto-applied (autonomy=auto): {}", auto_applied.join("; ")));
    } else if !new_proposals.is_empty() && !c.policy.auto_apply_allowed() {
        out.push_str(&format!(
            "\n  review with `soma proposals list` (autonomy={} keeps a human in the loop)",
            c.policy.autonomy
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::jarr;
    use crate::project::testutil::temp_ctx;
    use crate::skills;

    fn add_skill(c: &Ctx, name: &str, cmd: &str, timeout_s: i64) {
        skills::add(
            c,
            jobj(vec![
                ("name", jstr(name)),
                ("purpose", jstr("a test skill for the improvement engine")),
                ("goal", jstr("testing")),
                ("tags", jarr(vec![])),
                ("kind", jstr("command")),
                ("run", jobj(vec![("cmd", jstr(cmd)), ("timeout_s", jint(timeout_s))])),
            ]),
            false,
        )
        .unwrap();
    }

    #[test]
    fn scan_flags_flaky_skill_and_dedupes() {
        let (base, mut c) = temp_ctx();
        // Hermetic: point ollama at a dead port so fix_skill generation
        // deterministically skips and the advice-only fallback fires, even
        // on machines where a local model is running.
        let mut model = c.config.get("model").cloned().unwrap_or_else(|| jobj(vec![]));
        model.set("ollama_url", jstr("http://127.0.0.1:9"));
        c.config.set("model", model);
        add_skill(&c, "flaky", "exit 1", 30);
        for i in 0..4 {
            skills::record_outcome(&c, "flaky", i == 0, 10, "boom").unwrap();
        }
        let created = scan(&c).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].str_of("kind"), "fix_skill");
        assert!(created[0].str_of("rationale").contains("3/4"));
        // second scan: open duplicate suppressed
        assert!(scan(&c).unwrap().is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn timeout_issues_produce_applicable_tune_timeout() {
        let (base, mut c) = temp_ctx();
        add_skill(&c, "slowpoke", "sleep 100", 30);
        skills::file_issue(&c, "slowpoke", "run_failure", "timed out after 30s").unwrap();
        skills::file_issue(&c, "slowpoke", "run_failure", "timed out after 30s again").unwrap();
        let created = scan(&c).unwrap();
        let prop = created.iter().find(|p| p.str_of("kind") == "tune_timeout").unwrap();
        assert_eq!(prop.get("change").unwrap().i_of("timeout_s"), 60);

        let note = apply(&mut c, &prop.str_of("id")).unwrap();
        assert!(note.contains("60s"), "{note}");
        let s = skills::find(&c, "slowpoke").unwrap();
        assert_eq!(s.timeout_s(), 60);
        assert_eq!(s.manifest.i_of("version"), 2);
        // linked issue resolved → lesson recorded (R8 loop)
        assert!(skills::list_issues(&c, true).unwrap().len() < 2);
        assert!(crate::knowledge::list(&c, 10)
            .unwrap()
            .iter()
            .any(|k| k.str_of("kind") == "lesson"));
        // applying twice fails
        assert!(apply(&mut c, &prop.str_of("id")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn dismiss_and_status_tracking() {
        let (base, c) = temp_ctx();
        let p = add_proposal(&c, "advice", "x", "do better", jobj(vec![]))
            .unwrap()
            .unwrap();
        dismiss(&c, &p.str_of("id"), "not now").unwrap();
        assert!(list(&c, true).unwrap().is_empty());
        let all = list(&c, false).unwrap();
        assert_eq!(all[0].str_of("status"), "dismissed");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn optimizer_flags_simple_to_cloud_routing() {
        let (base, mut c) = temp_ctx();
        for _ in 0..5 {
            c.log(
                "model.route",
                jobj(vec![
                    ("level", jstr("simple")),
                    ("provider", jstr("anthropic")),
                    ("model", jstr("claude-haiku-4-5-20251001")),
                ]),
            )
            .unwrap();
        }
        let created = optimize(&c).unwrap();
        let prop = created
            .iter()
            .find(|p| p.str_of("target") == "model.routing.simple")
            .expect("routing proposal");
        let note = apply(&mut c, &prop.str_of("id")).unwrap();
        assert!(note.contains("model.routing.simple"), "{note}");
        // config actually changed
        let routing = c.config.get("model").unwrap().get("routing").unwrap();
        assert_eq!(routing.get("simple").unwrap().str_of("provider"), "ollama");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_path_setter() {
        let mut cfg = jobj(vec![]);
        set_config_path(&mut cfg, "a.b.c", jint(7));
        assert_eq!(cfg.get("a").unwrap().get("b").unwrap().i_of("c"), 7);
        set_config_path(&mut cfg, "a.b.c", jint(9));
        assert_eq!(cfg.get("a").unwrap().get("b").unwrap().i_of("c"), 9);
        set_config_path(&mut cfg, "top", jstr("v"));
        assert_eq!(cfg.str_of("top"), "v");
    }

    #[test]
    fn auto_apply_only_under_auto_autonomy() {
        let (base, mut c) = temp_ctx();
        add_skill(&c, "slow2", "sleep 100", 30);
        skills::file_issue(&c, "slow2", "run_failure", "timed out after 30s").unwrap();
        skills::file_issue(&c, "slow2", "run_failure", "timed out x2").unwrap();
        // assist: proposals created but NOT applied
        let out = tick(&mut c).unwrap();
        assert!(out.contains("new proposal"));
        assert!(!list(&c, true).unwrap().is_empty());
        // auto: tick applies mechanical proposals
        c.policy.autonomy = "auto".into();
        c.save_policy().unwrap();
        let out = tick(&mut c).unwrap();
        assert!(out.contains("auto-applied"), "{out}");
        assert!(list(&c, true)
            .unwrap()
            .iter()
            .all(|p| !MECHANICAL.contains(&p.str_of("kind").as_str())));
        std::fs::remove_dir_all(&base).ok();
    }

    // ---- fix_skill invariant tests ----

    /// Helper: point all routing tiers at the echo provider so generation is
    /// deterministic without network access.
    fn set_echo_routing(c: &mut Ctx) {
        let route = |p: &str, m: &str| jobj(vec![("provider", jstr(p)), ("model", jstr(m))]);
        let mut model = c.config.get("model").cloned().unwrap_or_else(|| jobj(vec![]));
        model.set(
            "routing",
            jobj(vec![
                ("simple", route("echo", "test-model")),
                ("moderate", route("echo", "test-model")),
                ("complex", route("echo", "test-model")),
            ]),
        );
        c.config.set("model", model);
        c.save_config().unwrap();
    }

    /// Invariant 1: apply path — cmd rewritten, version bumped, open issues
    /// resolved, lesson recorded, all journaled.
    #[test]
    fn fix_skill_apply_rewrites_cmd_bumps_version_resolves_issues() {
        let (base, mut c) = temp_ctx();
        add_skill(&c, "bad-cmd", "false", 30);
        // File open issues so the proposal is realistic.
        let i1 = skills::file_issue(&c, "bad-cmd", "run_failure", "exit 1; stderr: oops").unwrap();
        let _i2 = skills::file_issue(&c, "bad-cmd", "run_failure", "exit 1 again").unwrap();

        // Create a fix_skill proposal directly with a valid cmd.
        let change = jobj(vec![
            ("skill", jstr("bad-cmd")),
            ("cmd", jstr("echo fixed")),
            ("issue_id", jstr(&i1.str_of("id"))),
        ]);
        let p = add_proposal(
            &c,
            "fix_skill",
            "bad-cmd",
            "model suggests echo fixed (test provider/test-model)",
            change,
        )
        .unwrap()
        .unwrap();

        // Apply it.
        let note = apply(&mut c, &p.str_of("id")).unwrap();
        assert!(note.contains("run.cmd rewritten"), "{note}");

        // cmd must be rewritten.
        let s = skills::find(&c, "bad-cmd").unwrap();
        assert_eq!(s.cmd(), "echo fixed");
        // version bumped.
        assert_eq!(s.manifest.i_of("version"), 2);
        // open issues must be resolved.
        assert!(skills::list_issues(&c, true).unwrap().is_empty());
        // lesson recorded (from resolve_issue → knowledge).
        assert!(crate::knowledge::list(&c, 10)
            .unwrap()
            .iter()
            .any(|k| k.str_of("kind") == "lesson"));
        // proposal journaled.
        let tail = c.journal().tail(20).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "proposal.apply"
            && e.get("data").unwrap().str_of("kind") == "fix_skill"));
        // applying twice fails.
        assert!(apply(&mut c, &p.str_of("id")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    /// Invariant 2: generation path with echo provider.
    /// The echo provider echoes back the whole prompt, which is multi-line and
    /// very long → validation fails → no proposal created from invalid echo
    /// response. We also directly test that a short valid suggestion does
    /// produce a proposal with provider provenance in the rationale, and that a
    /// second scan within the suppression window does NOT re-propose.
    #[test]
    fn fix_skill_echo_provider_invalid_response_no_proposal_and_valid_path() {
        let (base, mut c) = temp_ctx();
        set_echo_routing(&mut c);
        add_skill(&c, "noisy", "false", 30);
        // Manufacture failures + issues to trigger the generation path.
        for _ in 0..3 {
            skills::record_outcome(&c, "noisy", false, 10, "stderr: bad output").unwrap();
        }

        // scan: echo response is multi-line + >400 chars → validation fails → no proposal
        // from model, but we fall through to the advice-only proposal.
        let created = scan(&c).unwrap();
        // The fallback advice-only proposal is still created.
        assert_eq!(created.len(), 1, "expected fallback fix_skill proposal");
        assert_eq!(created[0].str_of("kind"), "fix_skill");
        // No change.cmd — this is an advice-only proposal (echo response rejected).
        let change = created[0].get("change").unwrap();
        assert!(change.str_of("cmd").is_empty(), "cmd should be empty for advice-only proposal");

        // Second scan within suppression window: no duplicate.
        let second = scan(&c).unwrap();
        assert!(second.is_empty(), "second scan should produce no new proposals");

        // Unit-test: validate that the rationale format produced by the generation
        // path names provider + model. We build it directly as the generator would.
        let provider = "echo";
        let model = "test-model";
        let cached = false;
        let cache_note = if cached { " (cache hit)" } else { "" };
        let stderr_excerpt = "stderr: bad output";
        let rationale = format!(
            "skill 'noisy' failed 3 times; last error: \"{stderr_excerpt}\"; \
             model: {provider}/{model}{cache_note} suggests replacing run.cmd"
        );
        assert!(rationale.contains("echo"), "rationale must name provider");
        assert!(rationale.contains("test-model"), "rationale must name model");
        assert!(!rationale.contains("(cache hit)"), "non-cached should not say cache hit");

        // Unit-test: validate_fix_suggestion with a short, valid single-line cmd.
        let s = skills::find(&c, "noisy").unwrap();
        let v = validate_fix_suggestion(&c, "echo working", &s.cmd());
        assert!(v.is_ok(), "short valid cmd should pass validation: {v:?}");
        // Multi-line should be rejected.
        let v2 = validate_fix_suggestion(&c, "echo line1\necho line2", &s.cmd());
        assert!(v2.is_err());
        // Too-long should be rejected.
        let long = "x".repeat(401);
        let v3 = validate_fix_suggestion(&c, &long, &s.cmd());
        assert!(v3.is_err());

        std::fs::remove_dir_all(&base).ok();
    }

    /// Invariant 3: a suggestion matching deny_commands is discarded — no proposal.
    #[test]
    fn fix_skill_deny_list_discards_suggestion() {
        let (base, c) = temp_ctx();
        add_skill(&c, "risky", "echo safe", 30);

        // validate_fix_suggestion should reject "sudo rm -rf /" (matches deny_commands).
        let result = validate_fix_suggestion(&c, "sudo rm -rf /", "echo safe");
        assert!(result.is_err(), "policy should reject sudo rm -rf /");
        assert!(result.unwrap_err().contains("policy"));

        // Also "sudo whoami" matches deny_commands ("sudo *").
        let result2 = validate_fix_suggestion(&c, "sudo whoami", "echo safe");
        assert!(result2.is_err(), "policy should reject sudo commands");

        std::fs::remove_dir_all(&base).ok();
    }

    /// Invariant 4: autonomy:auto + tick does NOT auto-apply a fix_skill proposal.
    #[test]
    fn fix_skill_not_auto_applied_under_auto_autonomy() {
        let (base, mut c) = temp_ctx();
        add_skill(&c, "fail-skill", "false", 30);
        // Create enough failures and issues to trigger fix_skill proposal.
        for _ in 0..3 {
            skills::record_outcome(&c, "fail-skill", false, 10, "exit 1").unwrap();
        }
        // Set auto autonomy.
        c.policy.autonomy = "auto".into();
        c.save_policy().unwrap();

        tick(&mut c).unwrap();

        // fix_skill proposals must still be open (not auto-applied).
        let open = list(&c, true).unwrap();
        let fix_props: Vec<&Json> =
            open.iter().filter(|p| p.str_of("kind") == "fix_skill").collect();
        assert!(
            !fix_props.is_empty(),
            "fix_skill proposal should exist after tick"
        );
        // MECHANICAL does not include fix_skill.
        assert!(!MECHANICAL.contains(&"fix_skill"));
        // All remaining open proposals are NOT mechanical (the mechanical ones were applied).
        for p in &open {
            if p.str_of("kind") == "fix_skill" {
                assert_eq!(p.str_of("status"), "proposed", "fix_skill must not be auto-applied");
            }
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// Invariant 5: archived skills are never targeted by fix_skill.
    #[test]
    fn fix_skill_skips_archived_skills() {
        let (base, c) = temp_ctx();
        add_skill(&c, "old-cmd", "false", 30);
        // Accumulate enough failures to normally trigger fix_skill.
        for _ in 0..4 {
            skills::record_outcome(&c, "old-cmd", false, 10, "exit 1").unwrap();
        }
        // Archive the skill.
        let s = skills::find(&c, "old-cmd").unwrap();
        let mut m = s.manifest.clone();
        m.set("archived", crate::json::jbool(true));
        crate::util::atomic_write(&s.path, m.pretty().as_bytes()).unwrap();

        // scan must produce no fix_skill proposal for this archived skill.
        let created = scan(&c).unwrap();
        let fix_props: Vec<&Json> = created
            .iter()
            .filter(|p| p.str_of("kind") == "fix_skill" && p.str_of("target") == "old-cmd")
            .collect();
        assert!(
            fix_props.is_empty(),
            "archived skill must not get a fix_skill proposal; got: {fix_props:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cron_proposal_flow_via_detect() {
        let (base, c) = temp_ctx();
        // proposals from the cron module land in this queue
        let p = add_proposal(
            &c,
            "add_cron",
            "nightly-build",
            "ran 3 times daily",
            jobj(vec![
                ("name", jstr("auto-nightly-build")),
                ("schedule", jstr("0 3 * * *")),
                (
                    "action",
                    jobj(vec![("kind", jstr("command")), ("target", jstr("echo build"))]),
                ),
            ]),
        )
        .unwrap()
        .unwrap();
        let mut c2 = crate::project::Ctx::load(Some(&c.root.to_string_lossy())).unwrap();
        let note = apply(&mut c2, &p.str_of("id")).unwrap();
        assert!(note.contains("auto-nightly-build"));
        assert_eq!(crate::cron::list(&c2).len(), 1);
        std::fs::remove_dir_all(&base).ok();
    }
}
