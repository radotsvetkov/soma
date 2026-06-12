//! R13/R14 — crons: scheduled automation, and the proposer that notices
//! rhythms in your manual work and suggests automating them.
//!
//! Schedules are 5-field cron expressions evaluated in UTC
//! (minute hour day-of-month month day-of-week; `*` `,` `-` `/` supported).
//! There is no daemon: `soma tick` (run it from launchd/cron/systemd) runs
//! everything due. Vixie semantics for dom/dow: when both are restricted,
//! either may match.

use crate::json::{jbool, jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::util::*;

#[derive(Debug, Clone)]
pub struct CronSpec {
    pub minute: Vec<u32>,
    pub hour: Vec<u32>,
    pub dom: Vec<u32>,
    pub month: Vec<u32>,
    pub dow: Vec<u32>,
    dom_star: bool,
    dow_star: bool,
}

fn parse_field(field: &str, min: u32, max: u32) -> R<(Vec<u32>, bool)> {
    let mut vals: Vec<u32> = Vec::new();
    let is_star = field == "*";
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>().map_err(|_| format!("bad step in '{part}'"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step 0 in '{part}'"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else {
            match range.split_once('-') {
                Some((a, b)) => (
                    a.parse().map_err(|_| format!("bad range start '{a}'"))?,
                    b.parse().map_err(|_| format!("bad range end '{b}'"))?,
                ),
                None => {
                    let v: u32 = range.parse().map_err(|_| format!("bad value '{range}'"))?;
                    (v, v)
                }
            }
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("'{part}' outside {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            if !vals.contains(&v) {
                vals.push(v);
            }
            v += step;
        }
    }
    vals.sort_unstable();
    Ok((vals, is_star))
}

pub fn parse(expr: &str) -> R<CronSpec> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron needs 5 fields (minute hour dom month dow), got {} in '{expr}'",
            fields.len()
        ));
    }
    let (minute, _) = parse_field(fields[0], 0, 59).map_err(|e| format!("minute: {e}"))?;
    let (hour, _) = parse_field(fields[1], 0, 23).map_err(|e| format!("hour: {e}"))?;
    let (dom, dom_star) = parse_field(fields[2], 1, 31).map_err(|e| format!("day-of-month: {e}"))?;
    let (month, _) = parse_field(fields[3], 1, 12).map_err(|e| format!("month: {e}"))?;
    // Accept 7 as a Sunday alias: parse with 0-7 allowed, then fold any
    // resulting 7 to 0 in the VALUE SET. (A blanket `replace('7',"0")` on the
    // raw field would corrupt `*/7` → `*/0` and `5-7` → `5-0`.)
    let (mut dow, dow_star) =
        parse_field(fields[4], 0, 7).map_err(|e| format!("day-of-week: {e}"))?;
    for d in dow.iter_mut() {
        if *d == 7 {
            *d = 0;
        }
    }
    dow.sort_unstable();
    dow.dedup();
    Ok(CronSpec {
        minute,
        hour,
        dom,
        month,
        dow,
        dom_star,
        dow_star,
    })
}

impl CronSpec {
    pub fn matches(&self, p: &UtcParts) -> bool {
        if !self.minute.contains(&p.minute)
            || !self.hour.contains(&p.hour)
            || !self.month.contains(&p.month)
        {
            return false;
        }
        let dom_ok = self.dom.contains(&p.day);
        let dow_ok = self.dow.contains(&p.weekday);
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok, // vixie: both restricted → OR
        }
    }

    /// Next matching minute strictly after `from_ms` (UTC). None if nothing
    /// matches within 366 days (e.g. Feb 30).
    pub fn next_after(&self, from_ms: i64) -> Option<i64> {
        let mut t = (from_ms / 60_000 + 1) * 60_000; // next minute boundary
        for _ in 0..(366 * 24 * 60) {
            if self.matches(&utc_parts(t)) {
                return Some(t);
            }
            t += 60_000;
        }
        None
    }
}

// ---------- entries (R13) ----------

fn load_crons(c: &Ctx) -> Vec<Json> {
    read_to_string(&c.crons_path())
        .ok()
        .and_then(|s| crate::json::parse(&s).ok())
        .and_then(|j| j.arr().cloned())
        .unwrap_or_default()
}

fn save_crons(c: &Ctx, list: &[Json]) -> R<()> {
    atomic_write(
        &c.crons_path(),
        Json::Arr(list.to_vec()).pretty().as_bytes(),
    )
}

pub fn list(c: &Ctx) -> Vec<Json> {
    load_crons(c)
}

/// Add an entry. `action`: {kind: skill|goal|command, target, input?}.
pub fn add(c: &Ctx, name: &str, schedule: &str, action: Json) -> R<Json> {
    parse(schedule)?; // validate early
    let kind = action.str_of("kind");
    if !["skill", "goal", "command"].contains(&kind.as_str()) {
        return Err("action.kind must be skill|goal|command".into());
    }
    if action.str_of("target").is_empty() {
        return Err("action.target required (skill name / goal id / command line)".into());
    }
    let mut entries = load_crons(c);
    if entries.iter().any(|e| e.str_of("name") == name) {
        return Err(format!("cron '{name}' already exists"));
    }
    let entry = jobj(vec![
        ("id", jstr(new_id("cr"))),
        ("name", jstr(name)),
        ("schedule", jstr(schedule)),
        ("action", action),
        ("enabled", jbool(true)),
        ("created_iso", jstr(&iso8601(now_ms()))),
        ("last_run_ms", jint(0)),
        ("last_status", jstr("never")),
    ]);
    entries.push(entry.clone());
    save_crons(c, &entries)?;
    c.log(
        "cron.add",
        jobj(vec![("name", jstr(name)), ("schedule", jstr(schedule))]),
    )?;
    Ok(entry)
}

pub fn set_enabled(c: &Ctx, name: &str, enabled: bool) -> R<()> {
    let mut entries = load_crons(c);
    let e = entries
        .iter_mut()
        .find(|e| e.str_of("name") == name)
        .ok_or_else(|| format!("no cron '{name}'"))?;
    e.set("enabled", jbool(enabled));
    save_crons(c, &entries)?;
    c.log(
        "cron.toggle",
        jobj(vec![("name", jstr(name)), ("enabled", jbool(enabled))]),
    )?;
    Ok(())
}

/// Entries whose schedule matches the minute of `now_ms` and which haven't
/// already run in this minute.
pub fn due(c: &Ctx, now_ms_val: i64) -> Vec<Json> {
    let parts = utc_parts(now_ms_val);
    let this_minute = now_ms_val / 60_000;
    load_crons(c)
        .into_iter()
        .filter(|e| e.b_of("enabled"))
        .filter(|e| parse(&e.str_of("schedule")).map(|s| s.matches(&parts)).unwrap_or(false))
        .filter(|e| e.i_of("last_run_ms") / 60_000 != this_minute)
        .collect()
}

fn run_entry(c: &Ctx, entry: &Json) -> (bool, String) {
    let action = entry.get("action").cloned().unwrap_or_else(|| jobj(vec![]));
    let target = action.str_of("target");
    let input = action.str_of("input");
    // Each arm returns an EXPLICIT (ok, note). Earlier this sniffed the note
    // for "failed", which both passed a command that exited non-zero and
    // failed a goal merely titled with the word "failed" — corrected here.
    let result: R<(bool, String)> = match action.str_of("kind").as_str() {
        "skill" => crate::skills::find(c, &target).and_then(|s| {
            crate::skills::run(c, &s, if input.is_empty() { None } else { Some(&input) })
                .map(|o| (o.ok, format!("skill {} {}", target, if o.ok { "ok" } else { "failed" })))
        }),
        "goal" => crate::goals::run(c, &target, true)
            .map(|r| (r.ok, format!("goal '{}' {}", r.title, if r.ok { "done" } else { "failed" }))),
        "command" => crate::skills::exec_shell(c, &target, 300)
            .map(|r| (r.exit_code == 0 && !r.timed_out, format!("command exit {}", r.exit_code))),
        other => Err(format!("unknown action kind '{other}'")),
    };
    match result {
        Ok((ok, note)) => (ok, note),
        Err(e) => (false, e),
    }
}

/// Run everything due now. The scheduler's whole runtime contract: call this
/// at least once a minute (launchd/cron) or whenever you like (laptop use) —
/// the same-minute guard keeps it idempotent.
pub fn tick(c: &Ctx) -> R<Vec<(String, bool, String)>> {
    let now = now_ms();
    let due_now = due(c, now);
    let mut results = Vec::new();
    for entry in &due_now {
        let name = entry.str_of("name");
        let exec = c.policy.check_execution("cron.run");
        c.log("policy.decision", exec.to_json(&format!("cron.run:{name}")))?;
        let (ok, note) = if exec.allowed() {
            run_entry(c, entry)
        } else {
            (false, format!("blocked by policy ({})", exec.rule()))
        };
        c.log(
            "cron.run",
            jobj(vec![
                ("name", jstr(&name)),
                ("ok", jbool(ok)),
                ("note", jstr(&truncate_chars(&note, 200))),
            ]),
        )?;
        let mut entries = load_crons(c);
        if let Some(e) = entries.iter_mut().find(|e| e.str_of("name") == name) {
            e.set("last_run_ms", jint(now));
            e.set("last_status", jstr(if ok { "ok" } else { "failed" }));
        }
        save_crons(c, &entries)?;
        results.push((name, ok, note));
    }
    Ok(results)
}

// ---------- proposer (R14) ----------

/// Detect a regular cadence in run timestamps. Returns (schedule, human
/// description). Pure function — unit-tested directly.
pub fn detect_cadence(timestamps: &[i64]) -> Option<(String, String)> {
    if timestamps.len() < 3 {
        return None;
    }
    let mut ts = timestamps.to_vec();
    ts.sort_unstable();
    let intervals: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    let var = intervals.iter().map(|i| (i - mean) * (i - mean)).sum::<f64>()
        / intervals.len() as f64;
    let cv = var.sqrt() / mean;
    if cv > 0.35 {
        return None; // not regular enough to call a rhythm
    }
    let last = utc_parts(*ts.last().unwrap());
    let hour_ms = 3_600_000.0;
    if (0.75 * hour_ms..=1.5 * hour_ms).contains(&mean) {
        Some((
            format!("{} * * * *", last.minute),
            format!("hourly at minute {}", last.minute),
        ))
    } else if (20.0 * hour_ms..=28.0 * hour_ms).contains(&mean) {
        Some((
            format!("{} {} * * *", last.minute, last.hour),
            format!("daily at {:02}:{:02} UTC", last.hour, last.minute),
        ))
    } else if (6.0 * 24.0 * hour_ms..=8.0 * 24.0 * hour_ms).contains(&mean) {
        Some((
            format!("{} {} * * {}", last.minute, last.hour, last.weekday),
            format!(
                "weekly on day {} at {:02}:{:02} UTC",
                last.weekday, last.hour, last.minute
            ),
        ))
    } else {
        None
    }
}

/// Scan the journal for skills run manually on a regular rhythm and propose
/// crons for them (through the R7 proposal pipeline).
pub fn propose_crons(c: &Ctx) -> R<Vec<Json>> {
    let mut runs: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    c.journal().for_each(|ev| {
        if ev.str_of("kind") == "skill.run" {
            if let Some(d) = ev.get("data") {
                if d.b_of("ok") {
                    runs.entry(d.str_of("name")).or_default().push(ev.i_of("ts"));
                }
            }
        }
    })?;
    let existing: Vec<String> = load_crons(c)
        .iter()
        .map(|e| {
            e.get("action")
                .map(|a| a.str_of("target"))
                .unwrap_or_default()
        })
        .collect();
    let mut proposals = Vec::new();
    for (skill, ts) in runs {
        if existing.contains(&skill) {
            continue;
        }
        if let Some((schedule, desc)) = detect_cadence(&ts) {
            if let Some(p) = crate::improve::add_proposal(
                c,
                "add_cron",
                &skill,
                &format!(
                    "skill '{skill}' was run manually {} times at a regular cadence ({desc}) — automate it?",
                    ts.len()
                ),
                jobj(vec![
                    ("name", jstr(&format!("auto-{skill}"))),
                    ("schedule", jstr(&schedule)),
                    (
                        "action",
                        jobj(vec![("kind", jstr("skill")), ("target", jstr(&skill))]),
                    ),
                ]),
            )? {
                proposals.push(p);
            }
        }
    }
    Ok(proposals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    #[test]
    fn parse_and_expand_fields() {
        let s = parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(s.minute, vec![0, 15, 30, 45]);
        assert_eq!(s.hour, (9..=17).collect::<Vec<_>>());
        assert_eq!(s.dow, vec![1, 2, 3, 4, 5]);
        assert!(parse("61 * * * *").is_err());
        assert!(parse("* * * *").is_err());
        assert!(parse("a * * * *").is_err());
        assert!(parse("*/0 * * * *").is_err());
        // 7 == sunday alias
        assert_eq!(parse("0 0 * * 7").unwrap().dow, vec![0]);
    }

    #[test]
    fn dow_seven_alias_does_not_corrupt_steps_or_ranges() {
        // Regression: a blanket '7'->'0' string replace mangled these.
        assert_eq!(parse("0 0 * * */7").unwrap().dow, vec![0]); // every-7th from 0
        assert_eq!(parse("0 0 * * 5-7").unwrap().dow, vec![0, 5, 6]); // Fri-Sun (7 folds to 0)
        assert_eq!(parse("0 0 * * 1-7").unwrap().dow, vec![0, 1, 2, 3, 4, 5, 6]); // 7 folds, dedups with 0
        assert!(parse("0 0 * * 8").is_err()); // 8 still out of range
    }

    #[test]
    fn matching_and_vixie_dom_dow() {
        // 2026-06-10 is a Wednesday (weekday 3)
        let ms = (days_from_civil(2026, 6, 10) * 86400 + 9 * 3600 + 30 * 60) * 1000;
        let p = utc_parts(ms);
        assert!(parse("30 9 * * *").unwrap().matches(&p));
        assert!(parse("30 9 * * 3").unwrap().matches(&p));
        assert!(!parse("30 9 * * 4").unwrap().matches(&p));
        assert!(parse("*/10 * * * *").unwrap().matches(&p));
        // both dom and dow restricted: match if EITHER hits (dom=10 here)
        assert!(parse("30 9 10 * 5").unwrap().matches(&p));
        assert!(!parse("30 9 11 * 5").unwrap().matches(&p));
    }

    #[test]
    fn next_after_finds_next_slot() {
        // from 2026-06-10 09:30 UTC, next "0 9 * * *" is 09:00 the next day
        let from = (days_from_civil(2026, 6, 10) * 86400 + 9 * 3600 + 30 * 60) * 1000;
        let next = parse("0 9 * * *").unwrap().next_after(from).unwrap();
        let p = utc_parts(next);
        assert_eq!((p.day, p.hour, p.minute), (11, 9, 0));
        // impossible date → None
        assert!(parse("0 0 30 2 *").unwrap().next_after(from).is_none());
    }

    #[test]
    fn add_due_and_same_minute_guard() {
        let (base, c) = temp_ctx();
        add(
            &c,
            "every-minute",
            "* * * * *",
            jobj(vec![("kind", jstr("command")), ("target", jstr("echo tick"))]),
        )
        .unwrap();
        assert!(add(&c, "every-minute", "* * * * *", jobj(vec![("kind", jstr("command")), ("target", jstr("x"))])).is_err());
        assert!(add(&c, "bad", "99 * * * *", jobj(vec![("kind", jstr("command")), ("target", jstr("x"))])).is_err());

        let now = now_ms();
        assert_eq!(due(&c, now).len(), 1);
        // simulate "already ran this minute"
        let mut entries = load_crons(&c);
        entries[0].set("last_run_ms", jint(now));
        save_crons(&c, &entries).unwrap();
        assert!(due(&c, now).is_empty());
        // next minute it's due again
        assert_eq!(due(&c, now + 60_000).len(), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn success_detection_uses_exit_code_not_note_text() {
        let (base, c) = temp_ctx();
        // false positive guard: a command that exits non-zero must NOT be ok,
        // even though its note ('command exit 1') contains no 'failed'.
        add(
            &c,
            "exits-nonzero",
            "* * * * *",
            jobj(vec![("kind", jstr("command")), ("target", jstr("exit 7"))]),
        )
        .unwrap();
        let results = tick(&c).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].1, "non-zero exit must be recorded as failure: {results:?}");
        assert_eq!(load_crons(&c)[0].str_of("last_status"), "failed");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn tick_runs_due_entry_and_journals() {
        let (base, c) = temp_ctx();
        add(
            &c,
            "minutely-echo",
            "* * * * *",
            jobj(vec![("kind", jstr("command")), ("target", jstr("echo cron-ran"))]),
        )
        .unwrap();
        let results = tick(&c).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].1, "{results:?}");
        let entries = load_crons(&c);
        assert_eq!(entries[0].str_of("last_status"), "ok");
        assert!(entries[0].i_of("last_run_ms") > 0);
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "cron.run"));
        // immediate second tick: same-minute guard → nothing due
        assert!(tick(&c).unwrap().is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cadence_detection() {
        let day = 24 * 3_600_000i64;
        let base_ts = (days_from_civil(2026, 6, 1) * 86400 + 9 * 3600 + 15 * 60) * 1000;
        // four near-daily runs (±20min jitter)
        let daily: Vec<i64> = vec![
            base_ts,
            base_ts + day + 12 * 60_000,
            base_ts + 2 * day - 9 * 60_000,
            base_ts + 3 * day + 20 * 60_000,
        ];
        let (sched, desc) = detect_cadence(&daily).unwrap();
        assert!(desc.contains("daily"), "{desc}");
        assert!(parse(&sched).is_ok());

        // hourly
        let hourly: Vec<i64> = (0..4).map(|i| base_ts + i * 3_600_000).collect();
        assert!(detect_cadence(&hourly).unwrap().1.contains("hourly"));

        // irregular → none
        let irregular = vec![base_ts, base_ts + day, base_ts + day + 3_600_000, base_ts + 3 * day];
        assert!(detect_cadence(&irregular).is_none());
        // too few → none
        assert!(detect_cadence(&daily[..2]).is_none());
    }
}
