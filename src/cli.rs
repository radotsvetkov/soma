//! The soma CLI: thin, explicit dispatch over the library modules.
//! No arg-parsing dependency — flags are extracted by hand (R18) and every
//! command prints something a human can read and a script can grep.

use crate::json::{jarr, jbool, jint, jnum, jobj, jstr, Json};
use crate::project::Ctx;
use crate::util::*;

pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            0
        }
        Err(e) => {
            eprintln!("soma: {e}");
            1
        }
    }
}

/// Remove `--name value` from args, returning the value.
fn flag_val(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 < args.len() {
        let v = args.remove(i + 1);
        args.remove(i);
        Some(v)
    } else {
        args.remove(i);
        None
    }
}

/// Remove `--flag` from args, returning whether it was present.
fn flag_bool(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        true
    } else {
        false
    }
}

fn need(args: &[String], i: usize, what: &str) -> R<String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| format!("missing {what} — see `soma help`"))
}

fn load(project: Option<String>) -> R<Ctx> {
    Ctx::load(project.as_deref())
}

pub fn dispatch(mut args: Vec<String>) -> R<String> {
    // `--` is the end-of-options marker, but it is special ONLY for `wrap`
    // (D9): everything after it is the wrapped child command, verbatim, so the
    // child's own --help/--json/--project are not parsed as soma's. For every
    // OTHER command a literal `--` must NOT silently discard the tail (F4
    // regression) — we fold it back into the command's positional args.
    //
    // To decide which case applies we parse soma's globals only from the
    // segment BEFORE the first standalone `--`, determine the command, then
    // either hand the tail to wrap as the child or splice it back in.
    let tail = match args.iter().position(|a| a == "--") {
        Some(i) => {
            let tail = args.split_off(i + 1);
            args.pop(); // the `--` itself
            Some(tail)
        }
        None => None,
    };
    let project = flag_val(&mut args, "--project");
    // A trailing --help/-h on ANY subcommand prints help instead of running
    // it — subcommands otherwise ignore unknown args, so `soma export --help`
    // used to execute a real export (creating files). Guard globally. (Only
    // the before-`--` segment is inspected: a child's --help after `--` for
    // wrap belongs to the child, not soma.)
    if args.iter().any(|a| a == "--help" || a == "-h")
        && args.first().map(|s| s.as_str()) != Some("help")
    {
        return Ok(HELP.to_string());
    }
    let json = flag_bool(&mut args, "--json");
    let cmd = args.first().cloned().unwrap_or_else(|| "help".into());
    // wrap consumes the `--` tail as its child command; every other command
    // gets the tail spliced back onto its positional args so nothing is lost.
    let child = if cmd == "wrap" { tail.clone() } else { None };
    let mut rest = if args.is_empty() {
        vec![]
    } else {
        args[1..].to_vec()
    };
    if cmd != "wrap" {
        if let Some(t) = tail {
            rest.extend(t);
        }
    }
    match cmd.as_str() {
        "init" => cmd_init(rest, project),
        "status" => cmd_status(load(project)?, json),
        "log" => cmd_log(load(project)?, rest, json),
        "anchor" => cmd_anchor(load(project)?, rest, json),
        "export" => cmd_export(load(project)?, rest),
        "policy" => cmd_policy(load(project)?, rest),
        "preset" => cmd_preset(load(project)?, rest),
        "project" => cmd_project(rest, json),
        "skill" => cmd_skill(load(project)?, rest, json),
        "wrap" => cmd_wrap(load(project)?, rest, child.unwrap_or_default(), json),
        "issues" => cmd_issues(load(project)?, rest, json),
        "select" => cmd_select(load(project)?, rest, json),
        "model" => cmd_model(load(project)?, rest, json),
        "cache" => cmd_cache(load(project)?, rest, json),
        "goal" => cmd_goal(load(project)?, rest, json),
        "cron" => cmd_cron(load(project)?, rest, json),
        "tick" => {
            let mut c = load(project)?;
            crate::improve::tick(&mut c)
        }
        "proposals" => cmd_proposals(load(project)?, rest, json),
        "knowledge" => cmd_knowledge(load(project)?, rest, json),
        "mcp" => cmd_mcp(load(project)?, rest),
        "config" => cmd_config(load(project)?, rest, json),
        "optimize" => {
            let c = load(project)?;
            let created = crate::improve::optimize(&c)?;
            if created.is_empty() {
                Ok("optimizer: nothing to suggest right now".into())
            } else {
                Ok(format!(
                    "optimizer created {} proposal(s):\n{}",
                    created.len(),
                    render_proposals(&created)
                ))
            }
        }
        "version" | "--version" => {
            if json {
                Ok(jobj(vec![
                    ("version", jstr(crate::project::SOMA_VERSION)),
                    ("ui_api", jint(crate::project::UI_API as i64)),
                ])
                .to_string())
            } else {
                Ok(format!("soma {}", crate::project::SOMA_VERSION))
            }
        }
        "help" | "--help" | "-h" => Ok(HELP.to_string()),
        other => Err(format!("unknown command '{other}' — see `soma help`")),
    }
}

// ---------- commands ----------

fn cmd_init(mut args: Vec<String>, project: Option<String>) -> R<String> {
    let name = flag_val(&mut args, "--name");
    let with_builtins = flag_bool(&mut args, "--with-builtins");
    // Precedence: an explicit positional init dir > --project > cwd. When
    // --project is set it targets exactly that dir (no walking up cwd to detect
    // an unrelated parent project) — so `soma --project <dir> init` inits
    // <dir>, not the cwd or some ancestor.
    let dir = match (args.first(), project) {
        (Some(positional), _) => expand_home(positional),
        (None, Some(p)) => expand_home(&p),
        (None, None) => ctx(std::env::current_dir(), "cwd")?,
    };
    ensure_dir(&dir)?;
    let c = crate::project::init(&dir, name.as_deref())?;
    let mut out = format!(
        "initialized soma project '{}' at {}\n  journal: .soma/events.jsonl (hash-chained)\n  policy:  .soma/policy.json (autonomy: {})",
        c.name(),
        c.root.display(),
        c.policy.autonomy
    );
    if with_builtins {
        for m in crate::skills::builtin_manifests() {
            let name = m.str_of("name");
            crate::skills::add(&c, m, false)?;
            out.push_str(&format!("\n  skill installed: {name}"));
        }
    }
    out.push_str("\nnext: `soma skill list`, `soma select \"<task>\"`, `soma goal add`");
    Ok(out)
}

fn cmd_status(c: Ctx, json: bool) -> R<String> {
    let rep = c.journal().verify()?;
    let skills = crate::skills::load_all(&c);
    let issues = crate::skills::list_issues(&c, true)?;
    let proposals = crate::improve::list(&c, true)?;
    let goals = crate::goals::list(&c)?;
    let crons = crate::cron::list(&c);
    let cache = crate::cache::stats(&c);
    if json {
        // Build network object
        let network = jobj(vec![
            ("allow", jbool(c.policy.allow_network)),
            (
                "hosts",
                jarr(c.policy.allow_hosts.iter().map(|h| jstr(h)).collect()),
            ),
        ]);
        let obj = jobj(vec![
            ("project", jstr(&c.name())),
            ("root", jstr(c.root.to_string_lossy().as_ref())),
            ("autonomy", jstr(&c.policy.autonomy)),
            ("network", network),
            ("events", jint(rep.events as i64)),
            ("skills", jint(skills.len() as i64)),
            ("open_issues", jint(issues.len() as i64)),
            ("open_proposals", jint(proposals.len() as i64)),
            ("goals", jint(goals.len() as i64)),
            ("crons", jint(crons.len() as i64)),
        ]);
        return Ok(obj.to_string());
    }
    let mut out = format!(
        "project: {}  (root {})\npreset: {}   autonomy: {}   network: {}\n",
        c.name(),
        c.root.display(),
        c.config.str_of("preset"),
        c.policy.autonomy,
        if c.policy.allow_network {
            "allowed"
        } else {
            "local-only"
        },
    );
    out.push_str(&format!(
        "journal: {} events, chain {} (head {})\n",
        rep.events,
        if rep.ok { "intact ✓" } else { "BROKEN ✗" },
        truncate_chars(&rep.head, 12)
    ));
    out.push_str(&format!(
        "skills: {} ({} archived)   open issues: {}   open proposals: {}\n",
        skills.len(),
        skills.iter().filter(|s| s.archived()).count(),
        issues.len(),
        proposals.len()
    ));
    out.push_str(&format!(
        "goals: {} ({} done)   crons: {} ({} enabled)\n",
        goals.len(),
        goals
            .iter()
            .filter(|g| g.str_of("status") == "done")
            .count(),
        crons.len(),
        crons.iter().filter(|e| e.b_of("enabled")).count()
    ));
    out.push_str(&format!(
        "cache: {} entries, {}KB / {}KB, {} hits\n",
        cache.i_of("entries"),
        cache.i_of("bytes") / 1024,
        cache.i_of("max_bytes") / 1024,
        cache.i_of("hits_total")
    ));
    let tail = c.journal().tail(3)?;
    if !tail.is_empty() {
        out.push_str("recent events:\n");
        for ev in tail {
            out.push_str(&format!("  {}\n", crate::events::render_event(&ev)));
        }
    }
    Ok(out.trim_end().to_string())
}

fn cmd_log(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    // -n: absent → default 20; present but non-numeric or negative → loud error.
    // (`usize::from_str` rejects negatives and non-digits, so parse() does both.)
    let n: usize = match flag_val(&mut args, "-n") {
        None => 20,
        Some(v) => v
            .parse()
            .map_err(|_| format!("log tail: -n must be a non-negative integer, got '{v}'"))?,
    };
    match args.first().map(|s| s.as_str()).unwrap_or("tail") {
        "tail" => {
            // tail(0) currently returns ALL events (the VecDeque eviction guard
            // `len == 0` only triggers while empty), so -n 0 would dump the whole
            // journal. 0 means 0 — short-circuit to an empty result.
            let events = if n == 0 {
                Vec::new()
            } else {
                c.journal().tail(n)?
            };
            if json {
                // NDJSON: one raw event JSON per line
                Ok(events
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                Ok(events
                    .iter()
                    .map(crate::events::render_event)
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
        "show" => {
            // Full pretty event by id, or the last n events in full.
            let id = args.get(1).cloned();
            match id {
                Some(id) => {
                    let mut found = None;
                    c.journal().for_each(|ev| {
                        if ev.str_of("id") == id {
                            found = Some(ev.clone());
                        }
                    })?;
                    match found {
                        None => Err(format!("no event with id {id}")),
                        Some(e) => {
                            if json {
                                Ok(e.to_string())
                            } else {
                                Ok(e.pretty().trim_end().to_string())
                            }
                        }
                    }
                }
                None => Ok(c
                    .journal()
                    .tail(n)?
                    .iter()
                    .map(|e| e.pretty())
                    .collect::<Vec<_>>()
                    .join("")),
            }
        }
        "verify" => {
            let rep = c.journal().verify()?;
            if json {
                if rep.ok {
                    Ok(jobj(vec![
                        ("ok", jbool(true)),
                        ("events", jint(rep.events as i64)),
                        ("head", jstr(&rep.head)),
                    ])
                    .to_string())
                } else {
                    let (broken_line, reason) = rep.first_bad.unwrap_or((0, "unknown".into()));
                    // Print JSON to stdout, then return Err so main exits non-zero.
                    // We print directly here because the Err path suppresses output.
                    println!(
                        "{}",
                        jobj(vec![
                            ("ok", jbool(false)),
                            ("events_checked", jint(rep.events as i64)),
                            ("broken_line", jint(broken_line as i64)),
                            ("reason", jstr(&reason)),
                        ])
                        .to_string()
                    );
                    Err("journal TAMPERED (see JSON above)".into())
                }
            } else if rep.ok {
                Ok(format!(
                    "journal OK — {} events, head {}",
                    rep.events,
                    truncate_chars(&rep.head, 16)
                ))
            } else {
                let (line, why) = rep.first_bad.unwrap_or((0, "unknown".into()));
                Err(format!("journal TAMPERED at line {line}: {why}"))
            }
        }
        other => Err(format!(
            "log: unknown subcommand '{other}' (tail|show|verify)"
        )),
    }
}

/// D10 — `soma anchor now|list|verify`: RFC 3161 timestamps over the journal
/// head. `now` refuses on a broken chain and gates the TSA host at egress;
/// `verify` reports the chain/tsr-file/openssl checks individually.
fn cmd_anchor(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "now" => {
            let url = flag_val(&mut args, "--url");
            let data = crate::anchor::now(&c, url.as_deref())?;
            if json {
                return Ok(data.to_string());
            }
            Ok(format!(
                "anchored journal head at seq {} ({})\n  TSA: {} → {}\n  stored: .soma/anchors/{} (+ .tsq)\n  journaled: journal.anchor\nverify anytime with `soma anchor verify {}` — third-party openssl commands in any export's VERIFY.md",
                data.i_of("seq"),
                truncate_chars(&data.str_of("head"), 16),
                data.str_of("url"),
                data.str_of("status"),
                data.str_of("tsr_file"),
                data.i_of("seq"),
            ))
        }
        "list" => {
            let events = crate::anchor::list(&c)?;
            if json {
                return Ok(jarr(events).to_string());
            }
            if events.is_empty() {
                return Ok("no anchors — `soma anchor now` timestamps the journal head at a TSA".into());
            }
            Ok(events
                .iter()
                .map(|e| {
                    let d = e.get("data").cloned().unwrap_or_else(|| jobj(vec![]));
                    format!(
                        "seq {:<6} {:<8} {}  {}  head {}  {}",
                        d.i_of("seq"),
                        d.str_of("status"),
                        e.str_of("iso"),
                        d.str_of("url"),
                        truncate_chars(&d.str_of("head"), 12),
                        if d.str_of("status") == "granted" {
                            d.str_of("tsr_file")
                        } else {
                            truncate_chars(&d.str_of("reason"), 60)
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "verify" => {
            let all = flag_bool(&mut args, "--all");
            let seq_arg = args.get(1).cloned();
            let anchors = crate::anchor::list(&c)?;
            let granted: Vec<Json> = anchors
                .iter()
                .filter_map(|e| e.get("data").cloned())
                .filter(|d| d.str_of("status") == "granted")
                .collect();
            if granted.is_empty() {
                return Err("no granted anchors to verify — `soma anchor now` first".into());
            }
            let targets: Vec<Json> = if all {
                granted
            } else {
                match seq_arg {
                    Some(s) => {
                        let seq: i64 = s
                            .parse()
                            .map_err(|_| format!("anchor verify: '{s}' is not a sequence number"))?;
                        let found = granted
                            .into_iter()
                            .filter(|d| d.i_of("seq") == seq)
                            .next_back()
                            .ok_or_else(|| format!("no granted anchor at seq {seq} — `soma anchor list`"))?;
                        vec![found]
                    }
                    // default: the most recent granted anchor
                    None => vec![granted.last().unwrap().clone()],
                }
            };
            let reports: Vec<Json> = targets
                .iter()
                .map(|d| crate::anchor::verify_anchor(&c, d))
                .collect();
            let all_ok = reports.iter().all(|r| r.b_of("ok"));
            if json {
                let out = if reports.len() == 1 && !all {
                    reports[0].to_string()
                } else {
                    jarr(reports.clone()).to_string()
                };
                if all_ok {
                    return Ok(out);
                }
                // JSON to stdout, then non-zero exit (log verify precedent).
                println!("{out}");
                return Err("anchor verification FAILED (see JSON above)".into());
            }
            let text = reports
                .iter()
                .map(crate::anchor::render_verify)
                .collect::<Vec<_>>()
                .join("\n")
                .trim_end()
                .to_string();
            if all_ok {
                Ok(text)
            } else {
                Err(text)
            }
        }
        other => Err(format!("anchor: unknown subcommand '{other}' (now|list|verify)")),
    }
}

fn cmd_export(c: Ctx, mut args: Vec<String>) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "verify" => {
            let dir = need(&args, 1, "bundle directory")?;
            crate::export::verify_bundle(&expand_home(&dir))
        }
        "otlp" => {
            args.remove(0); // consume "otlp"
            let out = flag_val(&mut args, "--out");
            let path = crate::export::export_otlp(&c, out.as_deref())?;
            Ok(format!(
                "exported OTLP/JSON trace:\n  {}\nnext: akmon otel import {} --journal <dir>",
                path.display(),
                path.display()
            ))
        }
        "eu-ai-act" => {
            args.remove(0); // consume "eu-ai-act"
            let out = flag_val(&mut args, "--out");
            let (md, json) = crate::aiact::export_aiact(&c, out.as_deref())?;
            Ok(format!(
                "generated EU AI Act Article 12 logging annex:\n  {}\n  {}\nread the caveats on page 1 — this is NOT a conformity assessment and performs\nno Article 6 classification; operator fields via `soma config set aiact.<key>`",
                md.display(),
                json.display()
            ))
        }
        "attestation" => {
            args.remove(0); // consume "attestation"
            let subject = flag_val(&mut args, "--subject");
            let out = flag_val(&mut args, "--out");
            let path =
                crate::attest::export_attestation(&c, subject.as_deref(), out.as_deref())?;
            Ok(format!(
                "wrote in-toto Statement v1 (UNSIGNED — soma ships no signing, by design):\n  {}\nsubject digest = the journal head, a real sha256 over the hash-chained events\nsign in CI with cosign or `gh attestation` — see docs/CI.md",
                path.display()
            ))
        }
        _ => {
            let out = flag_val(&mut args, "--out");
            let dir = crate::export::export(&c, out.as_deref())?;
            Ok(format!(
                "exported evidence bundle:\n  {}\n  (+ .tar.gz next to it)\nverify anywhere with `soma export verify <dir>` — or by hand, see VERIFY.md inside",
                dir.display()
            ))
        }
    }
}

const POLICY_VALID_FIELDS: &[&str] = &[
    "autonomy",
    "allow_commands",
    "deny_commands",
    "mcp_allow_commands",
    "allow_network",
    "allow_hosts",
    "writable_paths",
    "redact_keys",
    "max_timeout_s",
];

fn cmd_policy(mut c: Ctx, mut args: Vec<String>) -> R<String> {
    // Consume optional --json flag
    let want_json = flag_bool(&mut args, "--json");
    match args.first().map(|s| s.as_str()).unwrap_or("show") {
        "show" => {
            if want_json {
                Ok(c.policy.to_json().to_string())
            } else {
                Ok(c.policy.to_json().pretty())
            }
        }
        "autonomy" => {
            let level = need(&args, 1, "autonomy level (observe|assist|auto)")?;
            if !crate::policy::AUTONOMY_LEVELS.contains(&level.as_str()) {
                // Journal the refused mutation (symmetry with `policy set`).
                let rule = format!(
                    "autonomy must be one of {:?}",
                    crate::policy::AUTONOMY_LEVELS
                );
                let _ = c.log(
                    "policy.decision",
                    jobj(vec![
                        ("subject", jstr(&format!("policy.autonomy:{level}"))),
                        ("allowed", jbool(false)),
                        ("rule", jstr(&rule)),
                    ]),
                );
                return Err(rule);
            }
            let old = c.policy.autonomy.clone();
            c.policy.autonomy = level.clone();
            c.save_policy()?;
            c.log(
                "policy.change",
                jobj(vec![
                    ("path", jstr("autonomy")),
                    ("old", jstr(&old)),
                    ("new", jstr(&level)),
                ]),
            )?;
            Ok(format!("autonomy: {old} → {level}"))
        }
        "set" => {
            let dotted = args.get(1).cloned().ok_or("policy set: missing <path>")?;
            let raw_val = args.get(2).cloned().ok_or("policy set: missing <value>")?;

            // Policy fields are FLAT — a dotted subpath (e.g. `autonomy.x`)
            // would object-replace and destroy the field on from_json reload.
            // Refuse it rather than silently wipe a security field.
            if dotted.contains('.') {
                let rule = format!(
                    "policy set: '{dotted}' — policy fields are flat; set the whole field (e.g. `policy set autonomy auto`)"
                );
                let _ = c.log(
                    "policy.decision",
                    jobj(vec![
                        ("subject", jstr(&format!("policy.set:{dotted}"))),
                        ("allowed", jbool(false)),
                        ("rule", jstr(&rule)),
                    ]),
                );
                return Err(rule);
            }
            let top = dotted.as_str();
            if !POLICY_VALID_FIELDS.contains(&top) {
                let rule = format!(
                    "policy set: unknown field '{}' — valid fields: {}",
                    top,
                    POLICY_VALID_FIELDS.join(", ")
                );
                let _ = c.log(
                    "policy.decision",
                    jobj(vec![
                        ("subject", jstr(&format!("policy.set:{dotted}"))),
                        ("allowed", jbool(false)),
                        ("rule", jstr(&rule)),
                    ]),
                );
                return Err(rule);
            }

            // Parse value: try JSON first, fall back to plain string.
            let new_val = crate::json::parse(&raw_val).unwrap_or_else(|_| Json::Str(raw_val));

            // For autonomy, validate the value.
            if top == "autonomy" {
                let level = match &new_val {
                    Json::Str(s) => s.clone(),
                    _ => {
                        let rule =
                            "policy set: autonomy value must be a string (observe|assist|auto)"
                                .to_string();
                        let _ = c.log(
                            "policy.decision",
                            jobj(vec![
                                ("subject", jstr(&format!("policy.set:{dotted}"))),
                                ("allowed", jbool(false)),
                                ("rule", jstr(&rule)),
                            ]),
                        );
                        return Err(rule);
                    }
                };
                if !crate::policy::AUTONOMY_LEVELS.contains(&level.as_str()) {
                    let rule = format!(
                        "policy set: autonomy must be one of {:?}",
                        crate::policy::AUTONOMY_LEVELS
                    );
                    let _ = c.log(
                        "policy.decision",
                        jobj(vec![
                            ("subject", jstr(&format!("policy.set:{dotted}"))),
                            ("allowed", jbool(false)),
                            ("rule", jstr(&rule)),
                        ]),
                    );
                    return Err(rule);
                }
            }

            // Apply by round-tripping through JSON.
            let mut policy_json = c.policy.to_json();
            let old_val = json_set_path(&mut policy_json, &[top], new_val.clone());

            // from_json now fails CLOSED on a present-but-wrong-type field, so
            // a value of the wrong shape (e.g. `deny_commands "x"` as a string)
            // surfaces as an Err here — reject the mutation, journal it, and
            // leave the policy unchanged rather than panicking or widening.
            let reconstructed = match crate::policy::Policy::from_json(&policy_json) {
                Ok(p) => p,
                Err(why) => {
                    let rule = format!(
                        "policy set: value for '{top}' is not valid for this field — not applied (policy unchanged): {why}"
                    );
                    let _ = c.log(
                        "policy.decision",
                        jobj(vec![
                            ("subject", jstr(&format!("policy.set:{dotted}"))),
                            ("allowed", jbool(false)),
                            ("rule", jstr(&rule)),
                        ]),
                    );
                    return Err(rule);
                }
            };

            // Reject schema-invalid values: if the field didn't actually take
            // (from_json fell back to a default), the write would silently
            // change — or LOOSEN — policy while reporting success.
            let effective = reconstructed.to_json();
            if json_get_path(&effective, &[top]) != Some(&new_val) {
                let rule = format!(
                    "policy set: value for '{top}' is not valid for this field — not applied (policy unchanged)"
                );
                let _ = c.log(
                    "policy.decision",
                    jobj(vec![
                        ("subject", jstr(&format!("policy.set:{dotted}"))),
                        ("allowed", jbool(false)),
                        ("rule", jstr(&rule)),
                    ]),
                );
                return Err(rule);
            }

            c.policy = reconstructed;
            c.save_policy()?;
            c.log(
                "policy.change",
                jobj(vec![
                    ("path", jstr(&dotted)),
                    ("old", old_val),
                    ("new", new_val),
                ]),
            )?;
            Ok(format!("policy: {dotted} updated"))
        }
        other => Err(format!(
            "policy: unknown subcommand '{other}' (show|autonomy|set)"
        )),
    }
}

fn cmd_preset(mut c: Ctx, args: Vec<String>) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => Ok(crate::project::presets()
            .iter()
            .map(|p| format!("{:<16} {}", p.name, p.description))
            .collect::<Vec<_>>()
            .join("\n")),
        "apply" => {
            let name = need(&args, 1, "preset name")?;
            crate::project::apply_preset(&mut c, &name)
        }
        other => Err(format!("preset: unknown subcommand '{other}' (list|apply)")),
    }
}

fn cmd_project(args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let list = crate::project::list_projects(&crate::project::soma_home());
            if json {
                return Ok(jarr(
                    list.iter()
                        .map(|p| {
                            jobj(vec![
                                ("name", jstr(&p.str_of("name"))),
                                ("root", jstr(&p.str_of("root"))),
                            ])
                        })
                        .collect(),
                )
                .to_string());
            }
            if list.is_empty() {
                return Ok("no projects registered — `soma init` one".into());
            }
            Ok(list
                .iter()
                .map(|p| format!("{:<20} {}", p.str_of("name"), p.str_of("root")))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        other => Err(format!("project: unknown subcommand '{other}' (list)")),
    }
}

fn cmd_skill(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let metrics = crate::skills::load_metrics(&c);
            let skills = crate::skills::load_all(&c);
            if json {
                let issues = crate::skills::list_issues(&c, true).unwrap_or_default();
                let arr: Vec<Json> = skills
                    .iter()
                    .map(|s| {
                        let m = metrics.get(&s.name());
                        let runs = m.map(|m| m.i_of("runs")).unwrap_or(0);
                        let ok = m.map(|m| m.i_of("ok")).unwrap_or(0);
                        let failures = m.map(|m| m.i_of("fail")).unwrap_or(0);
                        let last_used_ms = m.map(|m| m.i_of("last_used_ms")).unwrap_or(0);
                        let open_issues = issues
                            .iter()
                            .filter(|i| i.str_of("skill") == s.name())
                            .count();
                        let version = s.manifest.i_of("version");
                        jobj(vec![
                            ("name", jstr(&s.name())),
                            ("version", jint(version)),
                            ("kind", jstr(&s.kind())),
                            ("purpose", jstr(&s.purpose())),
                            ("goal", jstr(&s.goal())),
                            ("tags", jarr(s.tags().iter().map(|t| jstr(t)).collect())),
                            ("archived", jbool(s.archived())),
                            ("origin", jstr(s.scope)),
                            ("runs", jint(runs)),
                            ("successes", jint(ok)),
                            ("failures", jint(failures)),
                            (
                                "last_used_ms",
                                if last_used_ms > 0 {
                                    jint(last_used_ms)
                                } else {
                                    Json::Null
                                },
                            ),
                            ("open_issues", jint(open_issues as i64)),
                        ])
                    })
                    .collect();
                return Ok(jarr(arr).to_string());
            }
            if skills.is_empty() {
                return Ok(
                    "no skills — `soma skill add <manifest.json>` or `soma skill install-builtins`"
                        .into(),
                );
            }
            let mut out = format!(
                "{:<22} {:<8} {:<7} {:<12} purpose\n",
                "name", "scope", "kind", "record"
            );
            for s in skills {
                let m = metrics.get(&s.name());
                let record = m
                    .map(|m| format!("{}/{} ok", m.i_of("ok"), m.i_of("runs")))
                    .unwrap_or_else(|| "unused".into());
                out.push_str(&format!(
                    "{:<22} {:<8} {:<7} {:<12} {}{}\n",
                    s.name(),
                    s.scope,
                    s.kind(),
                    record,
                    truncate_chars(&s.purpose(), 60),
                    if s.archived() { " [archived]" } else { "" }
                ));
            }
            Ok(out.trim_end().to_string())
        }
        "show" => {
            let name = need(&args, 1, "skill name")?;
            let s = crate::skills::find(&c, &name)?;
            let metrics = crate::skills::load_metrics(&c);
            if json {
                let m = metrics.get(&name);
                let runs = m.map(|m| m.i_of("runs")).unwrap_or(0);
                let ok = m.map(|m| m.i_of("ok")).unwrap_or(0);
                let failures = m.map(|m| m.i_of("fail")).unwrap_or(0);
                let last_used_ms = m.map(|m| m.i_of("last_used_ms")).unwrap_or(0);
                let issues = crate::skills::list_issues(&c, true).unwrap_or_default();
                let open_issues = issues.iter().filter(|i| i.str_of("skill") == name).count();
                return Ok(jobj(vec![
                    ("manifest", s.manifest.clone()),
                    ("origin", jstr(s.scope)),
                    ("runs", jint(runs)),
                    ("successes", jint(ok)),
                    ("failures", jint(failures)),
                    (
                        "last_used_ms",
                        if last_used_ms > 0 {
                            jint(last_used_ms)
                        } else {
                            Json::Null
                        },
                    ),
                    ("open_issues", jint(open_issues as i64)),
                ])
                .to_string());
            }
            let mut out = s.manifest.pretty();
            if let Some(m) = metrics.get(&name) {
                out.push_str(&format!("metrics: {}\n", m.to_string()));
            }
            Ok(out.trim_end().to_string())
        }
        "add" => {
            let global = flag_bool(&mut args, "--global");
            let src = need(&args, 1, "manifest path (or '-' for stdin)")?;
            let content = if src == "-" {
                let mut buf = String::new();
                use std::io::Read;
                ctx(std::io::stdin().read_to_string(&mut buf), "read stdin")?;
                buf
            } else {
                read_to_string(&expand_home(&src))?
            };
            let manifest = crate::json::parse(&content).map_err(|e| format!("manifest: {e}"))?;
            let path = crate::skills::add(&c, manifest, global)?;
            Ok(format!("skill added: {}", path.display()))
        }
        "lint" => {
            let src = need(&args, 1, "manifest path")?;
            let manifest = crate::json::parse(&read_to_string(&expand_home(&src))?)
                .map_err(|e| format!("manifest: {e}"))?;
            let problems = crate::skills::lint(&manifest);
            if problems.is_empty() {
                Ok("manifest OK".into())
            } else {
                Err(format!("problems:\n  - {}", problems.join("\n  - ")))
            }
        }
        "run" => {
            // Input via --input, or positionally after the name — a bare
            // positional used to be silently ignored, which sent the literal
            // "{input}" placeholder into commands. Field-proven footgun.
            let input = flag_val(&mut args, "--input").or_else(|| args.get(2).cloned());
            let name = need(&args, 1, "skill name")?;
            let s = crate::skills::find(&c, &name)?;
            let out = crate::skills::run(&c, &s, input.as_deref())?;
            let mut text = format!(
                "skill {} → {} ({}ms, rule: {})\n",
                name,
                if out.ok { "ok" } else { "FAILED" },
                out.ms,
                out.success_rule
            );
            if !out.stdout.trim().is_empty() {
                text.push_str(&format!("--- stdout ---\n{}\n", out.stdout.trim_end()));
            }
            if !out.ok && !out.stderr.trim().is_empty() {
                text.push_str(&format!("--- stderr ---\n{}\n", out.stderr.trim_end()));
            }
            if out.ok {
                Ok(text.trim_end().to_string())
            } else {
                Err(text.trim_end().to_string())
            }
        }
        "install-builtins" => {
            let mut names = Vec::new();
            for m in crate::skills::builtin_manifests() {
                names.push(m.str_of("name"));
                crate::skills::add(&c, m, false)?;
            }
            Ok(format!("installed builtin skills: {}", names.join(", ")))
        }
        other => Err(format!(
            "skill: unknown subcommand '{other}' (list|show|add|lint|run|install-builtins)"
        )),
    }
}

/// D9 — `soma wrap [flags] -- <cmd> [args...]`: policy-gate the launch of any
/// agent CLI, tee+hash its output, journal receipts, propagate its exit code.
/// `child` is everything after `--`, verbatim (split off in `dispatch`).
fn cmd_wrap(c: Ctx, mut args: Vec<String>, child: Vec<String>, json: bool) -> R<String> {
    let label = flag_val(&mut args, "--label");
    let timeout_s: i64 = match flag_val(&mut args, "--timeout-s") {
        None => 0, // no timeout — wrapped agent sessions may be long-lived
        Some(v) => match v.parse() {
            Ok(t) if t > 0 => t,
            _ => return Err(format!("wrap: --timeout-s must be a positive integer, got '{v}'")),
        },
    };
    let cwd = flag_val(&mut args, "--cwd").map(|d| expand_home(&d));
    let env_strict = flag_bool(&mut args, "--env-strict");
    let mut env_pass = Vec::new();
    while let Some(v) = flag_val(&mut args, "--env-pass") {
        env_pass.push(v);
    }
    if let Some(stray) = args.first() {
        return Err(format!(
            "wrap: unexpected argument '{stray}' — usage: soma wrap [--label L] [--timeout-s N] [--cwd D] [--env-strict] [--env-pass NAME]... [--json] -- <cmd> [args...]"
        ));
    }
    let out = crate::wrap::run(
        &c,
        crate::wrap::WrapOpts {
            label,
            timeout_s,
            cwd,
            env_strict,
            env_pass,
            cmd: child,
        },
    )?;
    // Receipt: --json puts the wrap.end data (one object) on stdout; the
    // human one-liner goes to stderr so the child's stdout stays unpolluted.
    let d = &out.end;
    let receipt = format!(
        "wrap[{}]: exit {}{} in {}ms — stdout {}B sha256 {}, stderr {}B (wrap.start/wrap.end journaled)",
        d.str_of("label"),
        out.exit,
        if out.timed_out { " (timed out)" } else { "" },
        d.i_of("duration_ms"),
        d.i_of("stdout_bytes"),
        truncate_chars(&d.str_of("stdout_sha256"), 12),
        d.i_of("stderr_bytes"),
    );
    if out.soma_exit == 0 {
        if json {
            Ok(d.to_string())
        } else {
            eprintln!("{receipt}");
            Ok(String::new())
        }
    } else {
        // Propagate the child's exit code as soma's own — print + flush
        // first, since process::exit skips the normal return path.
        if json {
            println!("{}", d.to_string());
        } else {
            eprintln!("{receipt}");
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(out.soma_exit);
    }
}

fn cmd_issues(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "resolve" => {
            let note = flag_val(&mut args, "--note").unwrap_or_else(|| "resolved manually".into());
            let id = need(&args, 1, "issue id")?;
            crate::skills::resolve_issue(&c, &id, &note)?;
            Ok(format!("issue {id} resolved (lesson recorded)"))
        }
        _ => {
            let all = flag_bool(&mut args, "--all");
            let issues = crate::skills::list_issues(&c, !all)?;
            if json {
                // issues as stored
                return Ok(jarr(issues).to_string());
            }
            if issues.is_empty() {
                return Ok("no open issues".into());
            }
            Ok(issues
                .iter()
                .map(|i| {
                    format!(
                        "{}  [{}] {} — {} ({})",
                        i.str_of("id"),
                        i.str_of("status"),
                        i.str_of("skill"),
                        truncate_chars(&i.str_of("detail"), 80),
                        i.str_of("iso")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

fn cmd_select(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    let do_run = flag_bool(&mut args, "--run");
    let ask_model = flag_bool(&mut args, "--ask-model");
    let input = flag_val(&mut args, "--input");
    let top: usize = flag_val(&mut args, "--top")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let task = need(&args, 0, "task description (quoted)")?;

    // --json is incompatible with --run and --ask-model (dry by design)
    if json && (do_run || ask_model) {
        return Err("--json is incompatible with --run and --ask-model".into());
    }

    let sel = match crate::neuro::select(&c, &task) {
        Ok(sel) => sel,
        Err(e) => {
            // In --json mode, a skill-less project is a representable empty
            // result, not a failure: emit {chosen:null, candidates:[]} and
            // exit 0 so scripts get well-formed JSON. Other errors (e.g. empty
            // task) still propagate. Non-json keeps the helpful error.
            if json && e.contains("no skills registered") {
                return Ok(jobj(vec![
                    ("task", jstr(&task)),
                    ("chosen", Json::Null),
                    ("candidates", jarr(vec![])),
                ])
                .to_string());
            }
            return Err(e);
        }
    };

    if json {
        // Build candidate objects mirroring Selection/Candidate/Factor
        let cand_to_json = |cand: &crate::neuro::Candidate| {
            jobj(vec![
                ("name", jstr(&cand.name)),
                ("score", jnum(cand.score)),
                ("origin", jstr(&cand.scope)),
                ("kind", jstr(&cand.kind)),
                (
                    "factors",
                    jarr(
                        cand.factors
                            .iter()
                            .map(|f| {
                                jobj(vec![
                                    ("name", jstr(f.name)),
                                    ("value", jnum(f.value)),
                                    ("note", jstr(&f.note)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ])
        };
        let chosen = sel
            .candidates
            .first()
            .map(cand_to_json)
            .unwrap_or(Json::Null);
        let candidates: Vec<Json> = sel.candidates.iter().take(5).map(cand_to_json).collect();
        return Ok(jobj(vec![
            ("task", jstr(&sel.task)),
            ("chosen", chosen),
            ("candidates", jarr(candidates)),
        ])
        .to_string());
    }

    let mut rerank_note = None;
    let mut sel = sel;
    if ask_model {
        rerank_note = Some(crate::neuro::rerank_with_model(&c, &mut sel)?);
    }
    let mut out = crate::neuro::render(&sel, top);
    if let Some(note) = rerank_note {
        out.push_str(&format!("  {note}\n"));
    }
    if do_run {
        let topc = sel.candidates.first().ok_or("nothing to run")?;
        if topc.kind == "command" {
            let s = crate::skills::find(&c, &topc.name)?;
            let res = crate::skills::run(&c, &s, input.as_deref())?;
            out.push_str(&format!(
                "\nran {} → {} ({}ms)\n{}",
                topc.name,
                if res.ok { "ok" } else { "FAILED" },
                res.ms,
                truncate_chars(res.stdout.trim_end(), 2000)
            ));
        } else {
            out.push_str(&format!(
                "\nchosen skill {} is an MCP tool — call it with arguments:\n  soma mcp call <server> <tool> --json '{{...}}'",
                topc.name
            ));
        }
    }
    Ok(out.trim_end().to_string())
}

fn cmd_model(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let routing = c
                .config
                .get("model")
                .and_then(|m| m.get("routing"))
                .cloned()
                .unwrap_or_else(|| jobj(vec![]));
            let mut out = String::from("routing (difficulty → provider:model):\n");
            for level in ["simple", "moderate", "complex"] {
                let r = routing.get(level).cloned().unwrap_or_else(|| jobj(vec![]));
                let ready = crate::models::provider_ready(&c, &r.str_of("provider"));
                out.push_str(&format!(
                    "  {:<9} {}:{}  [{}]\n",
                    level,
                    r.str_of("provider"),
                    r.str_of("model"),
                    match ready {
                        Ok(()) => "ready".to_string(),
                        Err(e) => format!("unavailable: {e}"),
                    }
                ));
            }
            Ok(out.trim_end().to_string())
        }
        "probe" => {
            if json {
                let arr: Vec<Json> = crate::models::PROVIDERS
                    .iter()
                    .map(|p| match crate::models::provider_ready(&c, p) {
                        Ok(()) => jobj(vec![
                            ("provider", jstr(*p)),
                            ("ok", jbool(true)),
                            ("note", jstr("")),
                        ]),
                        Err(e) => jobj(vec![
                            ("provider", jstr(*p)),
                            ("ok", jbool(false)),
                            ("note", jstr(&e)),
                        ]),
                    })
                    .collect();
                return Ok(jarr(arr).to_string());
            }
            let mut out = String::new();
            for p in crate::models::PROVIDERS {
                out.push_str(&format!(
                    "{:<10} {}\n",
                    p,
                    match crate::models::provider_ready(&c, p) {
                        Ok(()) => "ready ✓".to_string(),
                        Err(e) => format!("not ready — {e}"),
                    }
                ));
            }
            Ok(out.trim_end().to_string())
        }
        "route" => {
            let task = need(&args, 1, "task (quoted)")?;
            let r = crate::models::route(&c, &task)?;
            if json {
                let factors: Vec<Json> = r
                    .factors
                    .iter()
                    .map(|(n, p)| jobj(vec![("points", jint(*p)), ("note", jstr(n))]))
                    .collect();
                return Ok(jobj(vec![
                    ("task", jstr(&task)),
                    ("difficulty", jstr(&r.level)),
                    ("points", jint(r.points)),
                    ("factors", jarr(factors)),
                    ("provider", jstr(&r.provider)),
                    ("model", jstr(&r.model)),
                ])
                .to_string());
            }
            Ok(crate::models::render_route(&r).trim_end().to_string())
        }
        "ask" => {
            let provider = flag_val(&mut args, "--provider");
            let model = flag_val(&mut args, "--model");
            let no_cache = flag_bool(&mut args, "--no-cache");
            let prompt = need(&args, 1, "prompt (quoted)")?;
            let reply = match (provider, model) {
                (Some(p), Some(m)) => {
                    if no_cache {
                        crate::models::ask(&c, &p, &m, &prompt)?
                    } else {
                        crate::models::ask_cached(&c, &p, &m, &prompt)?
                    }
                }
                _ => {
                    let (route, reply) = crate::models::ask_routed(&c, &prompt)?;
                    println!(
                        "[routed {} → {}:{}{}]",
                        route.level,
                        reply.provider,
                        reply.model,
                        route
                            .fallback_from
                            .as_ref()
                            .map(|f| format!(", fell back from {f}"))
                            .unwrap_or_default()
                    );
                    reply
                }
            };
            Ok(format!(
                "{}{}",
                reply.text.trim_end(),
                if reply.cached {
                    "\n[cache hit — zero cost]"
                } else {
                    ""
                }
            ))
        }
        other => Err(format!(
            "model: unknown subcommand '{other}' (list|probe|route|ask)"
        )),
    }
}

fn cmd_cache(c: Ctx, args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("stats") {
        "stats" => {
            let s = crate::cache::stats(&c);
            if json {
                // Return only the required fields (no "enabled" since not in spec)
                return Ok(jobj(vec![
                    ("entries", jint(s.i_of("entries"))),
                    ("bytes", jint(s.i_of("bytes"))),
                    ("max_bytes", jint(s.i_of("max_bytes"))),
                    ("hits_total", jint(s.i_of("hits_total"))),
                ])
                .to_string());
            }
            Ok(s.pretty().trim_end().to_string())
        }
        "clear" => {
            let n = crate::cache::clear(&c)?;
            Ok(format!("cleared {n} cache entries"))
        }
        other => Err(format!("cache: unknown subcommand '{other}' (stats|clear)")),
    }
}

fn cmd_goal(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let goals = crate::goals::list(&c)?;
            if json {
                return Ok(jarr(goals).to_string());
            }
            if goals.is_empty() {
                return Ok("no goals — `soma goal add \"title\" --why \"...\"`".into());
            }
            Ok(goals
                .iter()
                .map(|g| {
                    format!(
                        "{}  [{}] {} ({} steps)",
                        g.str_of("id"),
                        g.str_of("status"),
                        g.str_of("title"),
                        g.arr_of("steps").len()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "add" => {
            let why = flag_val(&mut args, "--why").unwrap_or_default();
            let accept = flag_val(&mut args, "--accept")
                .map(|a| {
                    a.split(';')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_else(Vec::new);
            let title = need(&args, 1, "goal title (quoted)")?;
            let g = crate::goals::add(&c, &title, &why, &accept)?;
            Ok(format!(
                "goal added: {} ({})\nnext: soma goal step {} --name <step> --kind command|skill|model --input \"...\"",
                g.str_of("id"),
                title,
                g.str_of("id")
            ))
        }
        "show" => {
            let id = need(&args, 1, "goal id")?;
            let g = crate::goals::get(&c, &id)?;
            if json {
                return Ok(g.to_string());
            }
            Ok(g.pretty().trim_end().to_string())
        }
        "status" => {
            // one goal's run status, or a roll-up of all
            match args.get(1) {
                Some(id) => {
                    let g = crate::goals::get(&c, id)?;
                    let mut out = format!(
                        "{} — {} ({} steps)\n",
                        g.str_of("title"),
                        g.str_of("status"),
                        g.arr_of("steps").len()
                    );
                    for r in g.arr_of("last_run") {
                        out.push_str(&format!(
                            "  [{}] {} — {}\n",
                            if r.b_of("ok") { "ok" } else { "FAIL" },
                            r.str_of("step"),
                            r.str_of("note")
                        ));
                    }
                    Ok(out.trim_end().to_string())
                }
                None => {
                    let goals = crate::goals::list(&c)?;
                    if goals.is_empty() {
                        return Ok("no goals".into());
                    }
                    Ok(goals
                        .iter()
                        .map(|g| format!("{:<10} {}", g.str_of("status"), g.str_of("title")))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }
        }
        "step" => {
            let name = flag_val(&mut args, "--name").ok_or("--name required")?;
            let kind = flag_val(&mut args, "--kind").unwrap_or_else(|| "command".into());
            let input = flag_val(&mut args, "--input").ok_or("--input required")?;
            let skill = flag_val(&mut args, "--skill");
            let verify = flag_val(&mut args, "--verify"); // "exit0" | "contains:X" | "command:X"
            let id = need(&args, 1, "goal id")?;
            let mut step = jobj(vec![
                ("name", jstr(&name)),
                ("kind", jstr(&kind)),
                ("input", jstr(&input)),
            ]);
            if let Some(s) = skill {
                step.set("skill", jstr(&s));
            }
            if let Some(v) = verify {
                let (vk, vv) = v.split_once(':').unwrap_or((v.as_str(), ""));
                step.set(
                    "verify",
                    jobj(vec![("kind", jstr(vk)), ("value", jstr(vv))]),
                );
            }
            crate::goals::add_step(&c, &id, step)?;
            Ok(format!("step '{name}' added to goal {id}"))
        }
        "run" => {
            let keep_going = flag_bool(&mut args, "--keep-going");
            let id = need(&args, 1, "goal id")?;
            let rep = crate::goals::run(&c, &id, !keep_going)?;
            let text = crate::goals::render_report(&rep).trim_end().to_string();
            if rep.ok {
                Ok(text)
            } else {
                Err(text)
            }
        }
        other => Err(format!(
            "goal: unknown subcommand '{other}' (list|add|show|status|step|run)"
        )),
    }
}

fn cmd_cron(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let entries = crate::cron::list(&c);
            if json {
                let now = now_ms();
                let arr: Vec<Json> = entries
                    .iter()
                    .map(|e| {
                        // Clone the stored entry then augment with next_iso
                        let next_iso = if e.b_of("enabled") {
                            crate::cron::parse(&e.str_of("schedule"))
                                .ok()
                                .and_then(|s| s.next_after(now))
                                .map(|ms| Json::Str(iso8601(ms)))
                                .unwrap_or(Json::Null)
                        } else {
                            Json::Null
                        };
                        let mut obj = e.clone();
                        obj.set("next_iso", next_iso);
                        obj
                    })
                    .collect();
                return Ok(jarr(arr).to_string());
            }
            if entries.is_empty() {
                return Ok("no crons — `soma cron add <name> \"<m h dom mon dow>\" --kind skill --target <skill>`".into());
            }
            let now = now_ms();
            Ok(entries
                .iter()
                .map(|e| {
                    let next = crate::cron::parse(&e.str_of("schedule"))
                        .ok()
                        .and_then(|s| s.next_after(now))
                        .map(iso8601)
                        .unwrap_or_else(|| "?".into());
                    format!(
                        "{:<20} {:<16} {:<9} last: {:<7} next: {}",
                        e.str_of("name"),
                        e.str_of("schedule"),
                        if e.b_of("enabled") {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        e.str_of("last_status"),
                        next
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "add" => {
            let kind = flag_val(&mut args, "--kind").unwrap_or_else(|| "skill".into());
            let target = flag_val(&mut args, "--target").ok_or("--target required")?;
            let input = flag_val(&mut args, "--input").unwrap_or_default();
            let name = need(&args, 1, "cron name")?;
            let schedule = need(&args, 2, "schedule (quoted, 5 fields)")?;
            let entry = crate::cron::add(
                &c,
                &name,
                &schedule,
                jobj(vec![
                    ("kind", jstr(&kind)),
                    ("target", jstr(&target)),
                    ("input", jstr(&input)),
                ]),
            )?;
            let next = crate::cron::parse(&schedule)
                .ok()
                .and_then(|s| s.next_after(now_ms()))
                .map(iso8601)
                .unwrap_or_default();
            Ok(format!(
                "cron '{}' added — next run {} UTC\nrun `soma tick` from launchd/cron at least once a minute, or whenever",
                entry.str_of("name"),
                next
            ))
        }
        "due" => {
            let due = crate::cron::due(&c, now_ms());
            Ok(if due.is_empty() {
                "nothing due this minute".into()
            } else {
                due.iter()
                    .map(|e| e.str_of("name"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "enable" | "disable" | "toggle" => {
            let name = need(&args, 1, "cron name")?;
            // toggle flips the current state — the cockpit's Enabled switch
            // doesn't know the current value, so it needs this.
            let enable = match args[0].as_str() {
                "enable" => true,
                "disable" => false,
                _ => {
                    let cur = crate::cron::list(&c)
                        .into_iter()
                        .find(|e| e.str_of("name") == name)
                        .map(|e| e.b_of("enabled"))
                        .ok_or_else(|| format!("cron '{name}' not found"))?;
                    !cur
                }
            };
            crate::cron::set_enabled(&c, &name, enable)?;
            Ok(format!(
                "cron '{name}' {}",
                if enable { "enabled" } else { "disabled" }
            ))
        }
        other => Err(format!(
            "cron: unknown subcommand '{other}' (list|add|due|enable|disable|toggle)"
        )),
    }
}

fn render_proposals(list: &[Json]) -> String {
    list.iter()
        .map(|p| {
            format!(
                "{}  [{}] {} — {}",
                p.str_of("id"),
                p.str_of("kind"),
                p.str_of("target"),
                truncate_chars(&p.str_of("rationale"), 100)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cmd_proposals(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let all = flag_bool(&mut args, "--all");
            let list = crate::improve::list(&c, !all)?;
            if json {
                return Ok(jarr(list).to_string());
            }
            if list.is_empty() {
                return Ok(
                    "no open proposals — `soma tick` or `soma optimize` may generate some".into(),
                );
            }
            Ok(render_proposals(&list))
        }
        "show" => {
            let id = need(&args, 1, "proposal id")?;
            let p = crate::improve::get(&c, &id)?;
            if json {
                return Ok(p.to_string());
            }
            Ok(p.pretty().trim_end().to_string())
        }
        "apply" => {
            let id = need(&args, 1, "proposal id")?;
            let mut c = c;
            let note = crate::improve::apply(&mut c, &id)?;
            if json {
                return Ok(jobj(vec![
                    ("ok", jbool(true)),
                    ("id", jstr(&id)),
                    ("action", jstr("applied")),
                    ("note", jstr(&note)),
                ])
                .to_string());
            }
            Ok(note)
        }
        "dismiss" => {
            let reason = flag_val(&mut args, "--reason").unwrap_or_else(|| "dismissed".into());
            let id = need(&args, 1, "proposal id")?;
            crate::improve::dismiss(&c, &id, &reason)?;
            if json {
                return Ok(jobj(vec![
                    ("ok", jbool(true)),
                    ("id", jstr(&id)),
                    ("action", jstr("dismissed")),
                    ("note", jstr(&reason)),
                ])
                .to_string());
            }
            Ok(format!("proposal {id} dismissed"))
        }
        other => Err(format!(
            "proposals: unknown subcommand '{other}' (list|show|apply|dismiss)"
        )),
    }
}

fn cmd_knowledge(c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("list") {
        "list" => {
            let entries = crate::knowledge::list(&c, 30)?;
            if json {
                return Ok(jarr(entries).to_string());
            }
            if entries.is_empty() {
                return Ok(
                    "knowledge base is empty — lessons accumulate as issues get resolved".into(),
                );
            }
            Ok(entries
                .iter()
                .map(|e| {
                    format!(
                        "{}  [{}] {} — {}",
                        e.str_of("id"),
                        e.str_of("kind"),
                        e.str_of("title"),
                        truncate_chars(&e.str_of("body"), 80)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "add" => {
            let tags: Vec<String> = flag_val(&mut args, "--tags")
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();
            let kind = need(&args, 1, "kind (lesson|note|reference)")?;
            let title = need(&args, 2, "title (quoted)")?;
            let body = need(&args, 3, "body (quoted)")?;
            let e = crate::knowledge::add(&c, &kind, &title, &body, &tags)?;
            Ok(format!("knowledge entry {} added", e.str_of("id")))
        }
        "search" => {
            let q = need(&args, 1, "query (quoted)")?;
            let hits = crate::knowledge::search(&c, &q, 10)?;
            if json {
                let arr: Vec<Json> = hits
                    .iter()
                    .map(|(score, e)| jobj(vec![("score", jnum(*score)), ("entry", e.clone())]))
                    .collect();
                return Ok(jarr(arr).to_string());
            }
            if hits.is_empty() {
                return Ok("no matches".into());
            }
            Ok(hits
                .iter()
                .map(|(score, e)| {
                    format!(
                        "{score:.2}  [{}] {} — {}",
                        e.str_of("kind"),
                        e.str_of("title"),
                        truncate_chars(&e.str_of("body"), 80)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        other => Err(format!(
            "knowledge: unknown subcommand '{other}' (list|add|search)"
        )),
    }
}

// ---------- dotted-path helpers (for config get/set) ----------

/// Walk a dotted path (e.g. "model.routing.simple.model") into a JSON tree,
/// returning a reference to the leaf value, or None.
fn json_get_path<'a>(root: &'a Json, path: &[&str]) -> Option<&'a Json> {
    let mut cur = root;
    for &key in path {
        cur = cur.get(key)?;
    }
    Some(cur)
}

/// Set a value at a dotted path, creating intermediate objects as needed.
/// Returns the old value at that path (or Json::Null if absent).
fn json_set_path(root: &mut Json, path: &[&str], value: Json) -> Json {
    if path.is_empty() {
        return Json::Null;
    }
    if path.len() == 1 {
        // Get old value before overwriting.
        let old = root.get(path[0]).cloned().unwrap_or(Json::Null);
        root.set(path[0], value);
        return old;
    }
    // Ensure intermediate object exists.
    if root.get(path[0]).is_none() {
        root.set(path[0], Json::Obj(vec![]));
    }
    // We need a mutable reference to the child. Extract, recurse, put back.
    let child = if let Json::Obj(pairs) = root {
        pairs.iter_mut().find(|(k, _)| k == path[0]).map(|(_, v)| v)
    } else {
        None
    };
    if let Some(child) = child {
        // If child is not an object, replace it.
        if !matches!(child, Json::Obj(_)) {
            *child = Json::Obj(vec![]);
        }
        json_set_path(child, &path[1..], value)
    } else {
        Json::Null
    }
}

fn cmd_config(mut c: Ctx, mut args: Vec<String>, json: bool) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "get" => {
            args.remove(0); // consume "get"
            // Path is the first remaining non-flag arg (if any).
            let path_arg = args.iter().find(|a| !a.starts_with('-')).cloned();
            match path_arg {
                None => {
                    // Print whole config.
                    if json {
                        Ok(c.config.to_string())
                    } else {
                        Ok(c.config.pretty().trim_end().to_string())
                    }
                }
                Some(ref dotted) => {
                    let parts: Vec<&str> = dotted.split('.').collect();
                    let val = json_get_path(&c.config, &parts)
                        .ok_or_else(|| format!("config: no value at '{dotted}'"))?;
                    if json {
                        Ok(val.to_string())
                    } else {
                        // Human-readable: for scalar values print raw, for objects pretty.
                        match val {
                            Json::Str(s) => Ok(s.clone()),
                            _ => Ok(val.pretty().trim_end().to_string()),
                        }
                    }
                }
            }
        }
        "set" => {
            let dotted = args.get(1).cloned().ok_or("config set: missing <path>")?;
            let raw_val = args.get(2).cloned().ok_or("config set: missing <value>")?;

            // Guardrail: refuse identity fields.
            let top = dotted.split('.').next().unwrap_or("");
            if top == "project" || top == "created" {
                let rule = format!(
                    "config set: cannot modify identity field '{top}' — create a new project instead"
                );
                let _ = c.log("policy.decision", jobj(vec![
                    ("subject", jstr(&format!("config.set:{dotted}"))),
                    ("allowed", jbool(false)),
                    ("rule", jstr(&rule)),
                ]));
                return Err(rule);
            }

            // Parse value: try JSON first, fall back to string.
            let new_val = crate::json::parse(&raw_val).unwrap_or_else(|_| Json::Str(raw_val));

            // anchor.* values are validated BEFORE the write (round-trip
            // precedent: `policy set`) — a bad value must not silently turn
            // anchoring off or point it at a nonsense endpoint.
            let refuse = |c: &Ctx, rule: String| -> R<String> {
                let _ = c.log(
                    "policy.decision",
                    jobj(vec![
                        ("subject", jstr(&format!("config.set:{dotted}"))),
                        ("allowed", jbool(false)),
                        ("rule", jstr(&rule)),
                    ]),
                );
                Err(rule)
            };
            if dotted == "anchor.auto" {
                let v = new_val.s().unwrap_or("");
                if v != "off" && v != "daily" {
                    return refuse(&c, format!(
                        "config set: anchor.auto must be \"off\" or \"daily\", got '{}'",
                        new_val.to_string()
                    ));
                }
            }
            if dotted == "anchor.tsa_url" {
                let ok = matches!(&new_val, Json::Str(s)
                    if crate::anchor::host_of_url(s).is_ok());
                if !ok {
                    return refuse(&c, format!(
                        "config set: anchor.tsa_url must be an http:// or https:// URL with a host, got '{}'",
                        new_val.to_string()
                    ));
                }
            }
            // aiact.* values feed the EU AI Act annex (D11) — validated the
            // same way: unknown keys, non-strings and empties are refused
            // before the write; the classification is a structured position.
            if let Some(key) = dotted.strip_prefix("aiact.") {
                if !crate::aiact::AIACT_KEYS.contains(&key) {
                    return refuse(&c, format!(
                        "config set: unknown aiact key '{dotted}' — valid: {}",
                        crate::aiact::AIACT_KEYS
                            .iter()
                            .map(|k| format!("aiact.{k}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let v = new_val.s().unwrap_or("");
                if v.trim().is_empty() {
                    return refuse(&c, format!(
                        "config set: {dotted} must be a non-empty string, got '{}'",
                        new_val.to_string()
                    ));
                }
                if key == "classification" && !crate::aiact::CLASSIFICATIONS.contains(&v) {
                    return refuse(&c, format!(
                        "config set: aiact.classification must be one of {} — got '{v}'",
                        crate::aiact::CLASSIFICATIONS.join(" | ")
                    ));
                }
            }

            let parts: Vec<&str> = dotted.split('.').collect();
            let old_val = json_set_path(&mut c.config, &parts, new_val.clone());

            c.save_config()?;
            c.log(
                "config.change",
                jobj(vec![
                    ("path", jstr(&dotted)),
                    ("old", old_val),
                    ("new", new_val),
                ]),
            )?;
            Ok(format!("config: {dotted} updated"))
        }
        _ => Err("config: unknown subcommand — use: get [dotted.path] [--json] | set <dotted.path> <value>".into()),
    }
}

fn cmd_mcp(c: Ctx, mut args: Vec<String>) -> R<String> {
    match args.first().map(|s| s.as_str()).unwrap_or("servers") {
        "servers" => {
            let servers = crate::mcp::list_servers(&c);
            if servers.is_empty() {
                return Ok(format!(
                    "no MCP servers — declare them in {}:\n{}",
                    c.mcp_path().display(),
                    r#"  {"servers": {"name": {"command": "...", "args": ["..."]}}}"#
                ));
            }
            Ok(servers
                .iter()
                .map(|(name, cfg)| {
                    format!(
                        "{:<16} {} {}",
                        name,
                        cfg.str_of("command"),
                        cfg.strs_of("args").join(" ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "add" => {
            let name = need(&args, 1, "server name")?;
            let command =
                flag_val(&mut args, "--cmd").ok_or("mcp add: --cmd <command> required")?;
            // F2/F3: reject an empty or flag-looking command before it can be
            // persisted (flag_val will hand back "" or a swallowed "--arg").
            if command.trim().is_empty() {
                return Err("mcp add: --cmd <command> must not be empty".into());
            }
            if command.starts_with('-') {
                return Err(format!(
                    "mcp add: --cmd value '{command}' looks like a flag — check the argument order (got `--cmd {command}`)"
                ));
            }
            // Collect all --arg values in order.
            let mut extra_args: Vec<String> = Vec::new();
            while let Some(v) = flag_val(&mut args, "--arg") {
                extra_args.push(v);
            }
            crate::mcp::add_server(&c, &name, &command, &extra_args)?;
            Ok(format!("mcp: server '{name}' added"))
        }
        "remove" => {
            let name = need(&args, 1, "server name")?;
            crate::mcp::remove_server(&c, &name)?;
            Ok(format!("mcp: server '{name}' removed"))
        }
        "tools" => {
            let server = need(&args, 1, "server name")?;
            let mut client = crate::mcp::McpClient::connect(&c, &server)?;
            let tools = client.tools_list(&c)?;
            Ok(tools
                .iter()
                .map(|t| {
                    format!(
                        "{:<20} {}",
                        t.str_of("name"),
                        truncate_chars(&t.str_of("description"), 80)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "call" => {
            let json_args = flag_val(&mut args, "--json").unwrap_or_else(|| "{}".into());
            let server = need(&args, 1, "server name")?;
            let tool = need(&args, 2, "tool name")?;
            let parsed = crate::json::parse(&json_args).map_err(|e| format!("--json: {e}"))?;
            crate::mcp::call(&c, &server, &tool, parsed)
        }
        "import" => {
            let server = need(&args, 1, "server name")?;
            let tools = crate::mcp::import(&c, &server)?;
            Ok(format!(
                "imported {} tool(s) from '{server}' as skills: {}\nthe selector can now rank them (`soma select ...`)",
                tools.len(),
                tools.join(", ")
            ))
        }
        other => Err(format!(
            "mcp: unknown subcommand '{other}' (servers|add|remove|tools|call|import)"
        )),
    }
}

const HELP: &str = r#"soma — transparent, policy-governed, self-improving agent runtime (zero deps)

usage: soma [--project <dir>] <command> ...

project
  init [dir] [--name N] [--with-builtins]   create .soma/ here (or in dir)
  status                                    one-screen overview
  project list                              all registered projects
  preset list | apply <name>                local-only / hybrid-default / cloud-max / low-ram
  policy show [--json] | autonomy <level> | set <field> <value>   observe | assist | auto

transparency (R1/R2)
  log tail [-n N] | log show [id] | log verify   hash-chained journal; verify detects tampering
  export [--out DIR] | export verify <dir>       portable evidence bundle (+ tar.gz)

compliance (v6/D11)
  export eu-ai-act [--out FILE.md]           EU AI Act Article 12 logging annex generated from
                                             the journal: markdown + machine-readable .json
                                             sibling (default exports/<project>-aiact-<stamp>).
                                             Refuses on a broken chain. Operator identity via
                                             config aiact.system|provider|deployer|
                                             intended_purpose|classification — missing values
                                             render as explicit placeholders, never omitted.
                                             limits: NOT a conformity assessment, NOT legal
                                             advice, performs no Article 6 classification
                                             (software development is not an Annex III area);
                                             eight caveats on page 1 of every generated annex.

attestation & CI (v6/D12)
  export attestation [--subject N] [--out F] in-toto Statement v1 JSON over the journal head
                                             (default exports/<project>-attestation-<stamp>.json).
                                             subject digest = the head hash — a real sha256 over
                                             the hash-chained events, so the digest binds the
                                             exact evidence chain. Refuses on a broken chain:
                                             chain.verified:true is never emitted falsely.
                                             limits: soma does NOT sign the statement (zero
                                             deps) — sign in CI with cosign or `gh attestation`.
                                             GitHub Action: ci/github-action (soma-governed-run);
                                             caller workflow + reviewer steps in docs/CI.md.

anchoring (v6/D10)
  anchor now [--url U]                       RFC 3161-timestamp the journal head at a TSA
                                             (default https://freetsa.org/tsr; alternate
                                             http://timestamp.digicert.com) — even the operator
                                             can't backdate the chain afterwards. Refuses on a
                                             broken chain; TSA host must pass the network policy
                                             (hybrid-default/cloud-max allow it; local-only never).
  anchor list                                all anchor attempts from the journal
  anchor verify [<seq>|--all]                recompute chain at seq + check stored .tsr + best-
                                             effort `openssl ts -verify`; reports each check.
      .tsq/.tsr archived under .soma/anchors/; exports bundle them with third-party
      openssl instructions in VERIFY.md. config: anchor.tsa_url, anchor.auto off|daily
      (daily: tick anchors when the last attempt is >24h old).
      limits: an anchor proves the head EXISTED at the TSA's time — it does not
      prove event contents are true, and it cannot retro-protect events never journaled.

govern any agent CLI (v6/D9)
  wrap [--label L] [--timeout-s N] [--cwd D] [--env-strict] [--env-pass NAME]...
       [--json] -- <cmd> [args...]
      policy-gates the spawn (autonomy + command deny globs), tees stdout/stderr
      live while sha256-hashing them, journals wrap.start/wrap.end receipts,
      and exits with the child's exit code (124 on --timeout-s kill).
      --env-strict passes only PATH HOME TERM LANG LC_* plus --env-pass names.
      limits: the child is NOT sandboxed — wrap gates the launch and records
      evidence; pipes degrade full-screen TUIs, so target headless/print modes
      (e.g. `claude -p`, `copilot -p`). Excerpt redaction is shape-based
      (name=value / name: value), not a content scanner — bare secrets in
      child output are not scrubbed. The command gate is a denylist of known-
      bad argv string forms, not a semantic guard (spacing/flag variants can
      evade it).

skills & selection (R4-R6)
  skill list | show <name> | run <name> [--input X]
  skill add <manifest.json|-> [--global] | lint <file> | install-builtins
  select "<task>" [--run] [--ask-model] [--top N]   explainable skill choice (the neuro selector)
  issues [--all] | issues resolve <id> [--note N]

models (R9-R11)
  model list | probe | route "<task>"       hybrid routing with explained difficulty
  model ask "<prompt>" [--provider P --model M] [--no-cache]
  cache stats | clear

direction & automation (R12-R15)
  goal add "<title>" [--why W] [--accept "a;b"]
  goal step <id> --name N --kind command|skill|model --input I [--skill S] [--verify contains:X]
  goal run <id> [--keep-going] | goal list | show <id> | status [id]
  cron list | add <name> "<5-field UTC>" --kind skill|goal|command --target T | due | enable|disable <name>
  tick                                      run due crons + generate proposals (+ auto-apply if autonomy=auto)
  proposals list [--all] | show|apply|dismiss <id>
  optimize                                  analyze journal/cache/routing → proposals

knowledge & integration (R8/R16)
  knowledge add <kind> "<title>" "<body>" [--tags a,b] | list | search "<q>"
  mcp servers | add <name> --cmd <cmd> [--arg <v>]... | remove <name> | tools <srv> | call <srv> <tool> [--json '{}'] | import <srv>

configuration
  config get [dotted.path] [--json]              read whole config or a nested value
  config set <dotted.path> <value>               write + journal (identity fields forbidden)

soma version | help

--json flag: version, status, log tail/show/verify, skill list/show, issues list,
             select, proposals list/show/apply/dismiss, model probe/route,
             cache stats, project list, knowledge list/search,
             goal list/show, cron list, config get, wrap,
             anchor now/list/verify"#;

// ---------- acceptance tests for --json ----------

#[cfg(test)]
mod json_tests {
    use super::*;
    use crate::json::{jint, jobj, jstr};
    use crate::project::testutil::temp_ctx;

    fn dispatch_json(args: &[&str]) -> Result<String, String> {
        dispatch(args.iter().map(|s| s.to_string()).collect())
    }

    fn add_test_skill(c: &crate::project::Ctx) {
        crate::skills::add(
            c,
            jobj(vec![
                ("name", jstr("test-skill")),
                ("purpose", jstr("test skill purpose for testing")),
                ("goal", jstr("run tests")),
                ("kind", jstr("command")),
                (
                    "run",
                    jobj(vec![("cmd", jstr("echo hello")), ("timeout_s", jint(5))]),
                ),
            ]),
            false,
        )
        .unwrap();
    }

    #[test]
    fn version_json() {
        let out = dispatch_json(&["version", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.str_of("version"), "0.2.0");
        assert_eq!(v.i_of("ui_api"), 1);
    }

    #[test]
    fn status_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "status", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(!v.str_of("project").is_empty());
        assert!(!v.str_of("root").is_empty());
        assert!(!v.str_of("autonomy").is_empty());
        assert!(v.get("network").is_some());
        assert!(v.get("network").unwrap().get("allow").is_some());
        assert!(v.get("events").is_some());
        assert!(v.get("skills").is_some());
        assert!(v.get("open_issues").is_some());
        assert!(v.get("open_proposals").is_some());
        assert!(v.get("goals").is_some());
        assert!(v.get("crons").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_tail_json_ndjson() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // journal already has project.init event
        let out = dispatch_json(&["--project", &root, "log", "tail", "-n", "5", "--json"]).unwrap();
        // NDJSON: each line must parse as valid JSON
        for line in out.lines() {
            if !line.is_empty() {
                crate::json::parse(line)
                    .unwrap_or_else(|e| panic!("bad ndjson line: {e} → {line}"));
            }
        }
        // Must have at least the init event
        assert!(!out.is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_show_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // get an event id from the tail
        let events = c.journal().tail(1).unwrap();
        let id = events[0].str_of("id");
        let out = dispatch_json(&["--project", &root, "log", "show", &id, "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.str_of("id"), id);
        assert!(v.get("kind").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_verify_json_ok() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "log", "verify", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.b_of("ok"), true);
        assert!(v.get("events").is_some());
        assert!(v.get("head").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_verify_json_broken_exits_nonzero_and_emits_json() {
        let (base, c) = temp_ctx();
        // tamper with the journal
        let content = std::fs::read_to_string(&c.journal().path).unwrap();
        let tampered = content.replace("project.init", "project.TAMPERED");
        std::fs::write(&c.journal().path, tampered).unwrap();
        let root = c.root.to_string_lossy().to_string();
        // The cmd prints JSON to stdout then returns Err (which causes non-zero exit)
        // We test the dispatch returns Err with the message we set
        let result = dispatch_json(&["--project", &root, "log", "verify", "--json"]);
        assert!(result.is_err(), "tampered journal must error");
        let msg = result.unwrap_err();
        assert!(msg.contains("TAMPERED"), "err message: {msg}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn skill_list_json() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "skill", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let s = &arr[0];
        assert!(!s.str_of("name").is_empty());
        assert!(s.get("version").is_some());
        assert!(s.get("kind").is_some());
        assert!(s.get("purpose").is_some());
        assert!(s.get("goal").is_some());
        assert!(s.get("tags").is_some());
        assert!(s.get("archived").is_some());
        assert!(s.get("origin").is_some());
        assert!(s.get("runs").is_some());
        assert!(s.get("successes").is_some());
        assert!(s.get("failures").is_some());
        assert!(s.get("open_issues").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn skill_show_json() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        let root = c.root.to_string_lossy().to_string();
        let out =
            dispatch_json(&["--project", &root, "skill", "show", "test-skill", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(v.get("manifest").is_some());
        assert_eq!(v.get("manifest").unwrap().str_of("name"), "test-skill");
        assert!(v.get("origin").is_some());
        assert!(v.get("runs").is_some());
        assert!(v.get("successes").is_some());
        assert!(v.get("failures").is_some());
        assert!(v.get("open_issues").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn issues_list_json() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        // File an issue manually
        crate::skills::file_issue(&c, "test-skill", "run_failure", "test failure").unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "issues", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        // Issue objects as stored must have id, skill, status
        let issue = &arr[0];
        assert!(!issue.str_of("id").is_empty());
        assert_eq!(issue.str_of("skill"), "test-skill");
        assert_eq!(issue.str_of("status"), "open");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn select_json() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        let root = c.root.to_string_lossy().to_string();
        let out =
            dispatch_json(&["--project", &root, "select", "test skill purpose", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(!v.str_of("task").is_empty());
        assert!(v.get("chosen").is_some());
        assert!(v.get("candidates").is_some());
        // chosen is not null when there's a skill
        assert!(!v.get("chosen").unwrap().is_null());
        let cand = v.get("chosen").unwrap();
        assert!(cand.get("name").is_some());
        assert!(cand.get("score").is_some());
        assert!(cand.get("origin").is_some());
        assert!(cand.get("kind").is_some());
        assert!(cand.get("factors").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn select_json_incompatible_with_run() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&[
            "--project",
            &root,
            "select",
            "test skill",
            "--json",
            "--run",
        ]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("incompatible"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn select_json_incompatible_with_ask_model() {
        let (base, c) = temp_ctx();
        add_test_skill(&c);
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&[
            "--project",
            &root,
            "select",
            "test skill",
            "--json",
            "--ask-model",
        ]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("incompatible"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn proposals_list_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Empty list is still a valid JSON array
        let out = dispatch_json(&["--project", &root, "proposals", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(v.arr().is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn proposals_show_apply_dismiss_json() {
        let (base, c) = temp_ctx();
        // Create a proposal
        let p = crate::improve::add_proposal(
            &c,
            "advice",
            "test-target",
            "test rationale",
            jobj(vec![]),
        )
        .unwrap()
        .unwrap();
        let id = p.str_of("id");
        let root = c.root.to_string_lossy().to_string();

        // show
        let out = dispatch_json(&["--project", &root, "proposals", "show", &id, "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.str_of("id"), id);

        // dismiss --json
        let out2 =
            dispatch_json(&["--project", &root, "proposals", "dismiss", &id, "--json"]).unwrap();
        let v2 = crate::json::parse(&out2).unwrap();
        assert_eq!(v2.b_of("ok"), true);
        assert_eq!(v2.str_of("id"), id);
        assert_eq!(v2.str_of("action"), "dismissed");
        assert!(v2.get("note").is_some());

        // Create another for apply
        let p2 = crate::improve::add_proposal(&c, "advice", "target2", "rationale2", jobj(vec![]))
            .unwrap()
            .unwrap();
        let id2 = p2.str_of("id");
        let out3 =
            dispatch_json(&["--project", &root, "proposals", "apply", &id2, "--json"]).unwrap();
        let v3 = crate::json::parse(&out3).unwrap();
        assert_eq!(v3.b_of("ok"), true);
        assert_eq!(v3.str_of("id"), id2);
        assert_eq!(v3.str_of("action"), "applied");
        assert!(v3.get("note").is_some());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn model_probe_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "model", "probe", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        // Must have echo, ollama, anthropic
        assert_eq!(arr.len(), 3);
        for item in arr {
            assert!(item.get("provider").is_some());
            assert!(item.get("ok").is_some());
            assert!(item.get("note").is_some());
        }
        // echo is always ok
        let echo = arr.iter().find(|i| i.str_of("provider") == "echo").unwrap();
        assert_eq!(echo.b_of("ok"), true);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn model_route_json() {
        let (base, mut c) = temp_ctx();
        // Configure echo routing so the test doesn't need a live ollama
        let route = |p: &str, m: &str| jobj(vec![("provider", jstr(p)), ("model", jstr(m))]);
        let mut model = c.config.get("model").cloned().unwrap();
        model.set(
            "routing",
            jobj(vec![
                ("simple", route("echo", "s")),
                ("moderate", route("echo", "m")),
                ("complex", route("echo", "x")),
            ]),
        );
        c.config.set("model", model);
        c.save_config().unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out =
            dispatch_json(&["--project", &root, "model", "route", "list files", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(!v.str_of("task").is_empty());
        assert!(!v.str_of("difficulty").is_empty());
        assert!(v.get("points").is_some());
        assert!(v.get("factors").is_some());
        assert!(!v.str_of("provider").is_empty());
        assert!(!v.str_of("model").is_empty());
        // difficulty must be one of simple|moderate|complex
        let diff = v.str_of("difficulty");
        assert!(
            ["simple", "moderate", "complex"].contains(&diff.as_str()),
            "difficulty: {diff}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cache_stats_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "cache", "stats", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(v.get("entries").is_some());
        assert!(v.get("bytes").is_some());
        assert!(v.get("max_bytes").is_some());
        assert!(v.get("hits_total").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn project_list_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "project", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let p = &arr[0];
        assert!(p.get("name").is_some());
        assert!(p.get("root").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn knowledge_list_json() {
        let (base, c) = temp_ctx();
        crate::knowledge::add(&c, "lesson", "test title", "test body about something", &[])
            .unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "knowledge", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let e = &arr[0];
        assert!(!e.str_of("id").is_empty());
        assert_eq!(e.str_of("kind"), "lesson");
        assert_eq!(e.str_of("title"), "test title");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn knowledge_search_json() {
        let (base, c) = temp_ctx();
        crate::knowledge::add(
            &c,
            "note",
            "rust testing tips",
            "use cargo test to run tests",
            &["rust".into()],
        )
        .unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&[
            "--project",
            &root,
            "knowledge",
            "search",
            "rust tests",
            "--json",
        ])
        .unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let hit = &arr[0];
        assert!(hit.get("score").is_some());
        assert!(hit.get("entry").is_some());
        let entry = hit.get("entry").unwrap();
        assert_eq!(entry.str_of("kind"), "note");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn goal_list_json() {
        let (base, c) = temp_ctx();
        crate::goals::add(&c, "test goal", "because testing", &["criterion1".into()]).unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "goal", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let g = &arr[0];
        assert!(!g.str_of("id").is_empty());
        assert_eq!(g.str_of("title"), "test goal");
        assert_eq!(g.str_of("status"), "open");
        assert!(g.get("steps").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn goal_show_json() {
        let (base, c) = temp_ctx();
        let g = crate::goals::add(&c, "show goal", "for showing", &[]).unwrap();
        let id = g.str_of("id");
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "goal", "show", &id, "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.str_of("id"), id);
        assert_eq!(v.str_of("title"), "show goal");
        assert!(v.get("steps").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cron_list_json() {
        let (base, c) = temp_ctx();
        crate::cron::add(
            &c,
            "test-cron",
            "0 9 * * *",
            jobj(vec![("kind", jstr("command")), ("target", jstr("echo hi"))]),
        )
        .unwrap();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "cron", "list", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        let arr = v.arr().unwrap();
        assert!(!arr.is_empty());
        let e = &arr[0];
        assert_eq!(e.str_of("name"), "test-cron");
        assert!(e.get("next_iso").is_some());
        // next_iso must be a non-empty string (cron is enabled, "0 9 * * *" has future matches)
        let next_iso = e.get("next_iso").unwrap();
        assert!(
            !next_iso.is_null(),
            "next_iso should not be null for enabled cron with future matches"
        );
        // It must be a Str variant
        assert!(
            matches!(next_iso, crate::json::Json::Str(_)),
            "next_iso must be a string, got: {}",
            next_iso.to_string()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- config tests ----------

    #[test]
    fn config_set_get_string_roundtrip() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Set a nested string value.
        dispatch_json(&[
            "--project",
            &root,
            "config",
            "set",
            "model.routing.simple.model",
            "llama3.2",
        ])
        .unwrap();
        // Get it back.
        let out = dispatch_json(&[
            "--project",
            &root,
            "config",
            "get",
            "model.routing.simple.model",
        ])
        .unwrap();
        assert_eq!(out, "llama3.2");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_set_get_numeric_stays_number() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Set a numeric value.
        dispatch_json(&[
            "--project",
            &root,
            "config",
            "set",
            "model.max_tokens",
            "2048",
        ])
        .unwrap();
        // Read back via --json and confirm it parses as a number, not a string.
        let out = dispatch_json(&[
            "--project",
            &root,
            "config",
            "get",
            "model.max_tokens",
            "--json",
        ])
        .unwrap();
        let val = crate::json::parse(&out).unwrap();
        assert!(val.f().is_some(), "expected JSON number, got: {out}");
        assert_eq!(val.i().unwrap(), 2048);
        // Also verify the config.json file on disk has it as a number.
        let cfg_raw = std::fs::read_to_string(c.dir.join("config.json")).unwrap();
        let cfg = crate::json::parse(&cfg_raw).unwrap();
        assert_eq!(cfg.get("model").unwrap().i_of("max_tokens"), 2048);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_set_journals_change_event() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Set a value and verify the journal has a config.change event.
        dispatch_json(&[
            "--project",
            &root,
            "config",
            "set",
            "cache.max_bytes",
            "1048576",
        ])
        .unwrap();
        let events = c.journal().tail(5).unwrap();
        let change = events.iter().find(|e| e.str_of("kind") == "config.change");
        assert!(change.is_some(), "expected config.change event in journal");
        let ev = change.unwrap();
        let data = ev.get("data").unwrap();
        assert_eq!(data.str_of("path"), "cache.max_bytes");
        // old should be the previous value (50 * 1024 * 1024 = 52428800)
        assert!(data.get("old").is_some());
        assert!(data.get("new").is_some());
        // new should be 1048576 as a number
        assert_eq!(data.get("new").unwrap().i().unwrap(), 1048576);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_set_guardrail_project_field() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "config", "set", "project", "hacked"]);
        assert!(result.is_err(), "setting 'project' must error");
        assert!(
            result.unwrap_err().contains("identity field"),
            "error must mention identity field"
        );
        // Also check 'created' prefix.
        let result2 = dispatch_json(&["--project", &root, "config", "set", "created", "1970"]);
        assert!(result2.is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_get_json_path() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Get an existing nested value as JSON.
        let out = dispatch_json(&[
            "--project",
            &root,
            "config",
            "get",
            "model.routing.simple.provider",
            "--json",
        ])
        .unwrap();
        let val = crate::json::parse(&out).unwrap();
        // Default config has provider = "ollama" for simple routing.
        assert_eq!(val.s().unwrap(), "ollama");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- policy tests ----------

    #[test]
    fn policy_show_json() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&["--project", &root, "policy", "show", "--json"]).unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(v.get("autonomy").is_some(), "missing autonomy");
        assert!(v.get("allow_commands").is_some(), "missing allow_commands");
        assert!(v.get("deny_commands").is_some(), "missing deny_commands");
        assert!(
            v.get("mcp_allow_commands").is_some(),
            "missing mcp_allow_commands"
        );
        assert!(v.get("allow_network").is_some(), "missing allow_network");
        assert!(v.get("allow_hosts").is_some(), "missing allow_hosts");
        assert!(v.get("writable_paths").is_some(), "missing writable_paths");
        assert!(v.get("redact_keys").is_some(), "missing redact_keys");
        assert!(v.get("max_timeout_s").is_some(), "missing max_timeout_s");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_set_autonomy_roundtrip_and_journal() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Set autonomy to "observe".
        let out =
            dispatch_json(&["--project", &root, "policy", "set", "autonomy", "observe"]).unwrap();
        assert!(out.contains("updated"), "unexpected output: {out}");
        // Read it back via show --json.
        let show = dispatch_json(&["--project", &root, "policy", "show", "--json"]).unwrap();
        let v = crate::json::parse(&show).unwrap();
        assert_eq!(v.str_of("autonomy"), "observe", "autonomy not persisted");
        // Journal must have a policy.change event.
        let events = c.journal().tail(10).unwrap();
        let change = events.iter().find(|e| e.str_of("kind") == "policy.change");
        assert!(change.is_some(), "no policy.change event in journal");
        let ev = change.unwrap();
        let data = ev.get("data").unwrap();
        assert_eq!(data.str_of("path"), "autonomy");
        assert!(data.get("old").is_some(), "missing old in journal data");
        assert!(data.get("new").is_some(), "missing new in journal data");
        assert_eq!(data.get("new").unwrap().s(), Some("observe"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_set_deny_commands_json_array() {
        let (base, _c) = temp_ctx();
        let root = _c.root.to_string_lossy().to_string();
        // Set deny_commands to a JSON array.
        dispatch_json(&[
            "--project",
            &root,
            "policy",
            "set",
            "deny_commands",
            r#"["sudo *","*rm -rf /*"]"#,
        ])
        .unwrap();
        let show = dispatch_json(&["--project", &root, "policy", "show", "--json"]).unwrap();
        let v = crate::json::parse(&show).unwrap();
        let arr = v.arr_of("deny_commands");
        assert_eq!(
            arr.len(),
            2,
            "deny_commands should have 2 entries, got: {:?}",
            arr
        );
        assert_eq!(arr[0].s(), Some("sudo *"));
        assert_eq!(arr[1].s(), Some("*rm -rf /*"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_set_unknown_field_refused() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "policy", "set", "hacker_field", "evil"]);
        assert!(result.is_err(), "unknown field must error");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("unknown field") || msg.contains("valid top-level"),
            "error: {msg}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_set_bad_autonomy_value_refused() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "policy", "set", "autonomy", "superauto"]);
        assert!(result.is_err(), "bad autonomy value must error");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("observe") || msg.contains("assist") || msg.contains("auto"),
            "error: {msg}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- refused-mutation journaling (hardening) ----------

    #[test]
    fn config_refused_identity_field_journals_decision() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Refused write must error...
        let result = dispatch_json(&["--project", &root, "config", "set", "project", "x"]);
        let err = result.unwrap_err();
        // ...and append a denied policy.decision reusing the existing event.
        let events = c.journal().tail(10).unwrap();
        let dec = events
            .iter()
            .find(|e| e.str_of("kind") == "policy.decision");
        assert!(
            dec.is_some(),
            "expected policy.decision event for refused config write"
        );
        let data = dec.unwrap().get("data").unwrap();
        assert_eq!(data.b_of("allowed"), false);
        assert_eq!(data.str_of("subject"), "config.set:project");
        // Honest audit trail: journaled rule == the error shown to the user.
        assert_eq!(data.str_of("rule"), err);
        assert!(data.str_of("rule").contains("cannot modify identity field"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_refused_write_leaves_field_unchanged() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Capture the real project name before the refused write.
        let before =
            dispatch_json(&["--project", &root, "config", "get", "project", "--json"]).unwrap();
        let before_name = crate::json::parse(&before)
            .unwrap()
            .s()
            .unwrap()
            .to_string();
        let _ = dispatch_json(&["--project", &root, "config", "set", "project", "hacked"]);
        let after =
            dispatch_json(&["--project", &root, "config", "get", "project", "--json"]).unwrap();
        let after_name = crate::json::parse(&after).unwrap().s().unwrap().to_string();
        assert_eq!(
            before_name, after_name,
            "refused config write must not change the field"
        );
        assert_ne!(after_name, "hacked");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_refused_bad_autonomy_journals_decision() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "policy", "set", "autonomy", "bad"]);
        assert!(result.is_err(), "bad autonomy must error");
        let err = result.unwrap_err();
        let events = c.journal().tail(10).unwrap();
        let dec = events
            .iter()
            .find(|e| e.str_of("kind") == "policy.decision");
        assert!(
            dec.is_some(),
            "expected policy.decision event for refused policy write"
        );
        let data = dec.unwrap().get("data").unwrap();
        assert_eq!(data.b_of("allowed"), false);
        assert_eq!(data.str_of("subject"), "policy.set:autonomy");
        // rule mirrors the human reason returned to the caller.
        assert_eq!(data.str_of("rule"), err);
        assert!(data.str_of("rule").contains("autonomy must be one of"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_refused_legacy_autonomy_journals_decision() {
        // The legacy `policy autonomy <bad>` subcommand must journal its
        // refusal too — symmetry with `policy set autonomy <bad>`.
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "policy", "autonomy", "bogus"]);
        let err = result.unwrap_err();
        let events = c.journal().tail(10).unwrap();
        let dec = events
            .iter()
            .find(|e| e.str_of("kind") == "policy.decision")
            .expect("refused legacy autonomy must journal a denial");
        let data = dec.get("data").unwrap();
        assert_eq!(data.b_of("allowed"), false);
        assert_eq!(data.str_of("subject"), "policy.autonomy:bogus");
        assert_eq!(data.str_of("rule"), err);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_refused_write_leaves_field_unchanged() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Default autonomy, captured before the refused write.
        let before = dispatch_json(&["--project", &root, "policy", "show", "--json"]).unwrap();
        let before_autonomy = crate::json::parse(&before).unwrap().str_of("autonomy");
        let _ = dispatch_json(&["--project", &root, "policy", "set", "autonomy", "bad"]);
        let after = dispatch_json(&["--project", &root, "policy", "show", "--json"]).unwrap();
        let after_autonomy = crate::json::parse(&after).unwrap().str_of("autonomy");
        assert_eq!(
            before_autonomy, after_autonomy,
            "refused policy write must not change autonomy"
        );
        assert_ne!(after_autonomy, "bad");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- log -n arg leniency (hardening) ----------

    #[test]
    fn log_tail_n_zero_returns_no_events() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Journal has at least the project.init event, so a leaky tail(0)
        // would dump everything. -n 0 must mean zero.
        let out = dispatch_json(&["--project", &root, "log", "tail", "-n", "0", "--json"]).unwrap();
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            0,
            "log tail -n 0 must return zero events, got: {out:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_tail_n_non_numeric_errors() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "log", "tail", "-n", "notanum"]);
        assert!(result.is_err(), "-n notanum must error");
        assert!(
            result.unwrap_err().contains("non-negative integer"),
            "error must explain -n"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn log_tail_n_negative_errors() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let result = dispatch_json(&["--project", &root, "log", "tail", "-n", "-1"]);
        assert!(result.is_err(), "-n -1 must error");
        assert!(
            result.unwrap_err().contains("non-negative integer"),
            "error must explain -n"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- select --json in a skill-less project (hardening) ----------

    #[test]
    fn select_json_empty_project_returns_representable_shape() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // No skills registered. --json must give the empty shape, exit 0.
        let out = dispatch_json(&[
            "--project",
            &root,
            "select",
            "do something useful",
            "--json",
        ])
        .unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert!(!v.str_of("task").is_empty());
        assert!(
            v.get("chosen").unwrap().is_null(),
            "chosen must be null with no skills"
        );
        let cands = v.get("candidates").unwrap().arr().unwrap();
        assert!(cands.is_empty(), "candidates must be empty with no skills");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- anchor (D10) ----------

    #[test]
    fn anchor_now_refused_under_local_only_via_cli() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // default policy is local-only → the TSA host must be refused
        let err = dispatch_json(&["--project", &root, "anchor", "now"]).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        // and the attempt trail is on the chain
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "journal.anchor"
            && e.get("data").unwrap().str_of("status") == "failed"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anchor_now_rejects_bad_url_before_any_gate() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let err = dispatch_json(&[
            "--project", &root, "anchor", "now", "--url", "ftp://nope",
        ])
        .unwrap_err();
        assert!(err.contains("http"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anchor_list_json_shape() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // empty → valid empty JSON array
        let out = dispatch_json(&["--project", &root, "anchor", "list", "--json"]).unwrap();
        assert!(crate::json::parse(&out).unwrap().arr().unwrap().is_empty());
        // a failed attempt (policy refusal) appears as a stored event
        let _ = dispatch_json(&["--project", &root, "anchor", "now"]);
        let out = dispatch_json(&["--project", &root, "anchor", "list", "--json"]).unwrap();
        let arr = crate::json::parse(&out).unwrap().arr().unwrap().clone();
        assert_eq!(arr.len(), 1);
        let d = arr[0].get("data").unwrap().clone();
        assert_eq!(d.str_of("status"), "failed");
        assert!(d.get("seq").is_some());
        assert!(d.get("head").is_some());
        assert!(d.get("url").is_some());
        // human list mentions the failure reason
        let human = dispatch_json(&["--project", &root, "anchor", "list"]).unwrap();
        assert!(human.contains("failed"), "{human}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anchor_verify_errors_without_granted_anchor() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let err = dispatch_json(&["--project", &root, "anchor", "verify"]).unwrap_err();
        assert!(err.contains("no granted anchors"), "{err}");
        let err = dispatch_json(&["--project", &root, "anchor", "verify", "notanum"]).unwrap_err();
        assert!(err.contains("no granted anchors"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- config anchor.* validation (D10) ----------

    #[test]
    fn config_anchor_auto_roundtrip_and_validation() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // default is present and "off"
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.auto"]).unwrap();
        assert_eq!(out, "off");
        // valid value round-trips
        dispatch_json(&["--project", &root, "config", "set", "anchor.auto", "daily"]).unwrap();
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.auto"]).unwrap();
        assert_eq!(out, "daily");
        // invalid value refused + journaled, field unchanged
        let err = dispatch_json(&["--project", &root, "config", "set", "anchor.auto", "weekly"])
            .unwrap_err();
        assert!(err.contains("off") && err.contains("daily"), "{err}");
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.auto"]).unwrap();
        assert_eq!(out, "daily", "refused write must not change the field");
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && e.get("data").unwrap().str_of("subject") == "config.set:anchor.auto"
            && !e.get("data").unwrap().b_of("allowed")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn config_anchor_tsa_url_roundtrip_and_validation() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // default present
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.tsa_url"]).unwrap();
        assert_eq!(out, "https://freetsa.org/tsr");
        // valid alternate round-trips
        dispatch_json(&[
            "--project", &root, "config", "set", "anchor.tsa_url", "http://timestamp.digicert.com",
        ])
        .unwrap();
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.tsa_url"]).unwrap();
        assert_eq!(out, "http://timestamp.digicert.com");
        // junk refused, field unchanged
        let err = dispatch_json(&[
            "--project", &root, "config", "set", "anchor.tsa_url", "not a url",
        ])
        .unwrap_err();
        assert!(err.contains("anchor.tsa_url"), "{err}");
        let out = dispatch_json(&["--project", &root, "config", "get", "anchor.tsa_url"]).unwrap();
        assert_eq!(out, "http://timestamp.digicert.com");
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- config aiact.* validation (D11) ----------

    #[test]
    fn config_aiact_roundtrip_and_validation() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // valid free-text field round-trips
        dispatch_json(&["--project", &root, "config", "set", "aiact.provider", "ACME GmbH"])
            .unwrap();
        let out = dispatch_json(&["--project", &root, "config", "get", "aiact.provider"]).unwrap();
        assert_eq!(out, "ACME GmbH");
        // unknown aiact key refused + journaled
        let err = dispatch_json(&["--project", &root, "config", "set", "aiact.bogus", "x"])
            .unwrap_err();
        assert!(err.contains("aiact.intended_purpose"), "lists valid keys: {err}");
        // empty value refused
        let err = dispatch_json(&["--project", &root, "config", "set", "aiact.deployer", ""])
            .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        // classification is a structured position: junk refused, enum accepted
        let err = dispatch_json(&[
            "--project", &root, "config", "set", "aiact.classification", "totally-fine",
        ])
        .unwrap_err();
        assert!(err.contains("out-of-scope"), "lists valid values: {err}");
        dispatch_json(&[
            "--project", &root, "config", "set", "aiact.classification", "out-of-scope",
        ])
        .unwrap();
        let out =
            dispatch_json(&["--project", &root, "config", "get", "aiact.classification"]).unwrap();
        assert_eq!(out, "out-of-scope");
        // refusals journaled at the mutation boundary (anchor.* precedent)
        let tail = c.journal().tail(10).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && e.get("data").unwrap().str_of("subject") == "config.set:aiact.bogus"
            && !e.get("data").unwrap().b_of("allowed")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn export_eu_ai_act_via_cli_writes_both_files() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // Inside the project root ({project}/* is in writable_paths, so the
        // --out path gate allows it).
        let out_dir = c.root.join("aiact_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let msg = dispatch_json(&[
            "--project",
            &root,
            "export",
            "eu-ai-act",
            "--out",
            &out_dir.to_string_lossy(),
        ])
        .unwrap();
        assert!(msg.contains("NOT a conformity assessment"), "{msg}");
        let mut mds: Vec<_> = std::fs::read_dir(&out_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        mds.sort();
        assert_eq!(mds.len(), 2, "md + json sibling: {mds:?}");
        assert!(mds.iter().any(|p| p.extension().map(|e| e == "md").unwrap_or(false)));
        assert!(mds.iter().any(|p| p.extension().map(|e| e == "json").unwrap_or(false)));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn tick_daily_auto_anchor_attempts_once_then_backs_off() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        dispatch_json(&["--project", &root, "config", "set", "anchor.auto", "daily"]).unwrap();
        // local-only policy → the attempt is refused at the gate (no egress,
        // hermetic) and journaled as a failed attempt.
        let out = dispatch_json(&["--project", &root, "tick"]).unwrap();
        assert!(out.contains("anchor.auto"), "tick must report the attempt: {out}");
        let anchors: Vec<_> = c
            .journal()
            .tail(50)
            .unwrap()
            .into_iter()
            .filter(|e| e.str_of("kind") == "journal.anchor")
            .collect();
        assert_eq!(anchors.len(), 1, "exactly one attempt");
        assert_eq!(anchors[0].get("data").unwrap().str_of("status"), "failed");
        // second tick within 24h: no new attempt (backoff until next day)
        let out2 = dispatch_json(&["--project", &root, "tick"]).unwrap();
        assert!(!out2.contains("anchor.auto"), "no retry within 24h: {out2}");
        let anchors2: Vec<_> = c
            .journal()
            .tail(50)
            .unwrap()
            .into_iter()
            .filter(|e| e.str_of("kind") == "journal.anchor")
            .collect();
        assert_eq!(anchors2.len(), 1, "still exactly one attempt");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn wrap_json_summary_is_wrap_end_data() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&[
            "--project", &root, "wrap", "--json", "--", "/bin/echo", "hi",
        ])
        .unwrap();
        let v = crate::json::parse(&out).unwrap();
        assert_eq!(v.str_of("label"), "echo");
        assert_eq!(v.i_of("exit"), 0);
        assert_eq!(v.i_of("stdout_bytes"), 3); // "hi\n"
        assert_eq!(v.str_of("stdout_sha256").len(), 64);
        assert!(!v.b_of("timed_out"));
        assert!(v.get("duration_ms").is_some());
        assert!(v.get("stdout_excerpt").is_some());
        assert!(v.get("stderr_excerpt").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn wrap_child_args_after_dashdash_are_verbatim() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // The child's own --help/--json must not be parsed as soma's: this
        // must RUN echo (journaling wrap.end), not print soma help.
        let out = dispatch_json(&[
            "--project", &root, "wrap", "--", "/bin/echo", "--help", "--json",
        ])
        .unwrap();
        assert!(out.is_empty(), "non-json wrap keeps stdout for the child");
        let tail = c.journal().tail(3).unwrap();
        let end = tail
            .iter()
            .find(|e| e.str_of("kind") == "wrap.end")
            .expect("wrap.end journaled");
        assert!(end
            .get("data")
            .unwrap()
            .str_of("stdout_excerpt")
            .contains("--help"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn wrap_refuses_empty_flag_looking_and_stray_args() {
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        // no `--` / empty child
        let err = dispatch_json(&["--project", &root, "wrap"]).unwrap_err();
        assert!(err.contains("missing child command"), "{err}");
        let err = dispatch_json(&["--project", &root, "wrap", "--"]).unwrap_err();
        assert!(err.contains("missing child command"), "{err}");
        // flag-looking child command (F2/F3 precedent)
        let err = dispatch_json(&["--project", &root, "wrap", "--", "-p"]).unwrap_err();
        assert!(err.contains("looks like a flag"), "{err}");
        // stray positional before `--` is a usage error, not silently ignored
        let err =
            dispatch_json(&["--project", &root, "wrap", "oops", "--", "/bin/echo"]).unwrap_err();
        assert!(err.contains("unexpected argument 'oops'"), "{err}");
        // bad --timeout-s is loud
        let err = dispatch_json(&[
            "--project", &root, "wrap", "--timeout-s", "abc", "--", "/bin/echo",
        ])
        .unwrap_err();
        assert!(err.contains("--timeout-s"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn dashdash_tail_only_special_for_wrap() {
        // F4 regression: a literal `--` on a NON-wrap command must fold its
        // tail back into the command's positional args, not silently discard
        // it. `knowledge add note <title> -- <body>` needs the body that sits
        // after `--`; discarding it (the bug) makes add fail "missing body".
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&[
            "--project", &root, "knowledge", "add", "note", "atitle", "--", "abody",
        ])
        .expect("tail after -- must NOT be lost for a non-wrap command");
        assert!(out.contains("added"), "{out}");
        // The body actually reached storage (not dropped).
        let entries = crate::knowledge::list(&c, 5).unwrap();
        assert!(
            entries.iter().any(|e| e.str_of("body") == "abody"),
            "body after -- must be stored"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn wrap_child_literal_json_not_consumed_by_soma() {
        // F4 requirement (a): wrap still works — the child's literal --json
        // after `--` is the child's, not soma's. echo prints `--json hi`; soma
        // does NOT emit a JSON wrap.end summary (that would be --json mode).
        let (base, c) = temp_ctx();
        let root = c.root.to_string_lossy().to_string();
        let out = dispatch_json(&[
            "--project", &root, "wrap", "--", "/bin/echo", "--json", "hi",
        ])
        .unwrap();
        assert!(out.is_empty(), "soma must not treat the child's --json as its own");
        let end = c
            .journal()
            .tail(3)
            .unwrap()
            .into_iter()
            .find(|e| e.str_of("kind") == "wrap.end")
            .expect("wrap.end journaled");
        // echo received `--json hi` verbatim.
        assert!(end.get("data").unwrap().str_of("stdout_excerpt").contains("--json hi"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn init_honors_project_flag_and_positional_precedence() {
        // F3: `soma --project <dir> init` inits exactly <dir> (not cwd, not an
        // ancestor). Positional init dir still wins over --project; bare init
        // still uses cwd.
        let stamp = crate::util::new_id("t");
        let pdir = std::env::temp_dir().join(format!("soma-initp-{stamp}"));
        let pos = std::env::temp_dir().join(format!("soma-initpos-{stamp}"));
        std::fs::remove_dir_all(&pdir).ok();
        std::fs::remove_dir_all(&pos).ok();

        // --project targets that dir exactly.
        let out = dispatch(
            ["--project", &pdir.to_string_lossy(), "init", "--name", "p"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .expect("init with --project must succeed");
        assert!(out.contains(&pdir.to_string_lossy().to_string()), "{out}");
        assert!(pdir.join(".soma").is_dir(), "--project dir must be initialized");

        // Explicit positional dir wins even when --project is also set.
        let out = dispatch(
            [
                "--project",
                &pdir.to_string_lossy(),
                "init",
                &pos.to_string_lossy(),
                "--name",
                "q",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        )
        .expect("positional init dir must succeed");
        assert!(pos.join(".soma").is_dir(), "positional dir must be initialized");
        assert!(out.contains(&pos.to_string_lossy().to_string()), "{out}");

        std::fs::remove_dir_all(&pdir).ok();
        std::fs::remove_dir_all(&pos).ok();
    }
}
