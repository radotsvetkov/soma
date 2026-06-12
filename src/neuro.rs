//! R6 — the neuro selector: given a task, find the best skill and *explain
//! the choice*.
//!
//! Scoring is deterministic and decomposed into named factors so the
//! explanation is the actual computation, not a story about it:
//!   text match      how much of the task the skill's name/purpose/goal covers
//!   tag match       explicit tag hits
//!   reliability     Laplace-smoothed success rate from real runs (R5)
//!   recency         small boost if recently exercised
//!   knowledge       lessons (R8) that match the task and mention the skill
//! An optional model re-rank can reorder the top candidates, but the factor
//! scores always remain visible — autonomy stays auditable.

use crate::json::{jarr, jint, jnum, jobj, jstr, Json};
use crate::project::Ctx;
use crate::skills::{load_all, load_metrics, Skill};
use crate::util::*;

#[derive(Debug)]
pub struct Factor {
    pub name: &'static str,
    pub value: f64,
    pub note: String,
}

#[derive(Debug)]
pub struct Candidate {
    pub name: String,
    pub kind: String,
    pub scope: String,
    pub score: f64,
    pub factors: Vec<Factor>,
}

#[derive(Debug)]
pub struct Selection {
    pub task: String,
    pub candidates: Vec<Candidate>, // sorted by score desc
}

const W_TEXT: f64 = 4.0;
const W_TAGS: f64 = 1.0; // per tag hit, capped at 3 hits
const W_RELIABILITY: f64 = 2.0;
const W_RECENCY: f64 = 0.5;
const W_KNOWLEDGE: f64 = 0.75; // per matching lesson, capped at 2

fn score_skill(c: &Ctx, task_tokens: &[String], task: &str, s: &Skill, metrics: &Json) -> Candidate {
    let mut factors = Vec::new();

    // text match — how much of the task do this skill's words cover?
    let text = format!("{} {} {}", s.name(), s.purpose(), s.goal());
    let skill_tokens = tokenize(&text);
    let hits: Vec<&String> = task_tokens
        .iter()
        .filter(|t| skill_tokens.contains(t))
        .collect();
    let coverage = hits.len() as f64 / task_tokens.len().max(1) as f64;
    factors.push(Factor {
        name: "text",
        value: coverage * W_TEXT,
        note: if hits.is_empty() {
            "no task words in purpose/goal".into()
        } else {
            format!(
                "covers {}/{} task words ({})",
                hits.len(),
                task_tokens.len(),
                hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )
        },
    });

    // tag match
    let tags = s.tags();
    let tag_hits: Vec<String> = tags
        .iter()
        .filter(|t| task_tokens.contains(&t.to_lowercase()))
        .cloned()
        .collect();
    factors.push(Factor {
        name: "tags",
        value: (tag_hits.len().min(3) as f64) * W_TAGS,
        note: if tag_hits.is_empty() {
            "no tag hits".into()
        } else {
            format!("matched: {}", tag_hits.join(", "))
        },
    });

    // reliability from metrics (Laplace-smoothed so unproven ≈ 0.5)
    let m = metrics.get(&s.name());
    let runs = m.map(|m| m.i_of("runs")).unwrap_or(0);
    let ok = m.map(|m| m.i_of("ok")).unwrap_or(0);
    let rate = (ok as f64 + 1.0) / (runs as f64 + 2.0);
    factors.push(Factor {
        name: "reliability",
        value: rate * W_RELIABILITY,
        note: if runs == 0 {
            "unproven (no runs yet)".into()
        } else {
            format!("{ok}/{runs} runs succeeded")
        },
    });

    // recency — linear decay over 7 days
    let last = m.map(|m| m.i_of("last_used_ms")).unwrap_or(0);
    let age_ms = (now_ms() - last).max(0);
    let week = 7 * 24 * 3600 * 1000i64;
    let rec = if last > 0 && age_ms < week {
        (1.0 - age_ms as f64 / week as f64) * W_RECENCY
    } else {
        0.0
    };
    factors.push(Factor {
        name: "recency",
        value: rec,
        note: if last == 0 {
            "never used".into()
        } else {
            format!("last used {}h ago", age_ms / 3_600_000)
        },
    });

    // knowledge — lessons matching the task that mention this skill
    let lessons = crate::knowledge::lessons_mentioning(c, task, &s.name()).unwrap_or_default();
    factors.push(Factor {
        name: "knowledge",
        value: (lessons.len().min(2) as f64) * W_KNOWLEDGE,
        note: if lessons.is_empty() {
            "no matching lessons".into()
        } else {
            format!("lessons: {}", lessons.join("; "))
        },
    });

    Candidate {
        name: s.name(),
        kind: s.kind(),
        scope: s.scope.to_string(),
        score: factors.iter().map(|f| f.value).sum(),
        factors,
    }
}

pub fn select(c: &Ctx, task: &str) -> R<Selection> {
    let task_tokens = tokenize(task);
    if task_tokens.is_empty() {
        return Err("task is empty — tell me what you want done".into());
    }
    let skills = load_all(c);
    if skills.is_empty() {
        return Err("no skills registered — add one with `soma skill add` or `soma init --with-builtins`".into());
    }
    let metrics = load_metrics(c);
    let mut candidates: Vec<Candidate> = skills
        .iter()
        .filter(|s| !s.archived())
        .map(|s| score_skill(c, &task_tokens, task, s, &metrics))
        .collect();
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Journal the decision with its full factor breakdown (R1+R6).
    if let Some(top) = candidates.first() {
        c.log(
            "select.explain",
            jobj(vec![
                ("task", jstr(task)),
                ("chosen", jstr(&top.name)),
                ("score", jnum(round2(top.score))),
                (
                    "factors",
                    jarr(
                        top.factors
                            .iter()
                            .map(|f| {
                                jobj(vec![
                                    ("name", jstr(f.name)),
                                    ("value", jnum(round2(f.value))),
                                    ("note", jstr(&f.note)),
                                ])
                            })
                            .collect(),
                    ),
                ),
                ("candidates", jint(candidates.len() as i64)),
            ]),
        )?;
    }
    Ok(Selection {
        task: task.to_string(),
        candidates,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Optional model re-rank (R6): ask the routed model to pick among the top
/// candidates. The factor scores stay visible either way; if the model names
/// a different winner it is moved to the front and the decision journaled.
/// Any model failure leaves the deterministic ranking untouched.
pub fn rerank_with_model(c: &Ctx, sel: &mut Selection) -> R<String> {
    let top: Vec<String> = sel.candidates.iter().take(3).map(|cand| cand.name.clone()).collect();
    if top.len() < 2 {
        return Ok("fewer than 2 candidates — nothing to re-rank".into());
    }
    let prompt = format!(
        "Task: {}\nCandidate tools (best-first by heuristic): {}\nReply with exactly one candidate name — the best tool for the task.",
        sel.task,
        top.join(", ")
    );
    // Model failure must NOT lose the deterministic ranking — fall back to it
    // and journal why (the order on screen is still the explainable one).
    let reply = match crate::models::ask_routed(c, &prompt) {
        Ok((_route, reply)) => reply,
        Err(e) => {
            let note = format!("model unavailable ({e}) — kept the deterministic ranking");
            c.log(
                "select.rerank",
                jobj(vec![("task", jstr(&sel.task)), ("note", jstr(&note))]),
            )?;
            return Ok(note);
        }
    };
    let picked = top
        .iter()
        .find(|name| reply.text.contains(name.as_str()))
        .cloned();
    let note = match picked {
        Some(name) if name != sel.candidates[0].name => {
            let pos = sel.candidates.iter().position(|cand| cand.name == name).unwrap_or(0);
            let cand = sel.candidates.remove(pos);
            sel.candidates.insert(0, cand);
            format!("model re-rank ({}:{}) promoted '{name}' over the heuristic winner", reply.provider, reply.model)
        }
        Some(name) => format!("model re-rank ({}:{}) agreed with '{name}'", reply.provider, reply.model),
        None => "model reply named no candidate — heuristic ranking kept".to_string(),
    };
    c.log(
        "select.rerank",
        jobj(vec![
            ("task", jstr(&sel.task)),
            ("note", jstr(&note)),
            ("reply_excerpt", jstr(&truncate_chars(&reply.text, 120))),
        ]),
    )?;
    Ok(note)
}

/// Human rendering for the CLI — the explanation *is* the score breakdown.
pub fn render(sel: &Selection, top_n: usize) -> String {
    let mut out = format!("task: \"{}\"\n", sel.task);
    match sel.candidates.first() {
        None => out.push_str("no candidates.\n"),
        Some(top) => {
            out.push_str(&format!(
                "→ chosen: {} (score {:.2}, {} skill, {})\n  because:\n",
                top.name, top.score, top.scope, top.kind
            ));
            for f in &top.factors {
                out.push_str(&format!("    {:<12} {:>5.2}  {}\n", f.name, f.value, f.note));
            }
            if let Some(second) = sel.candidates.get(1) {
                let strongest = second
                    .factors
                    .iter()
                    .max_by(|a, b| a.value.partial_cmp(&b.value).unwrap_or(std::cmp::Ordering::Equal));
                out.push_str(&format!(
                    "  runner-up: {} (score {:.2}) — strongest factor: {}\n",
                    second.name,
                    second.score,
                    strongest.map(|f| format!("{} {:.2}", f.name, f.value)).unwrap_or_default()
                ));
            }
            if sel.candidates.len() > 2 {
                out.push_str("  others:\n");
                for cand in sel.candidates.iter().skip(2).take(top_n.saturating_sub(2)) {
                    out.push_str(&format!("    {:<20} {:.2}\n", cand.name, cand.score));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{jarr as ja, jobj as o};
    use crate::project::testutil::temp_ctx;
    use crate::skills;

    fn add_skill(c: &Ctx, name: &str, purpose: &str, tags: Vec<&str>, cmd: &str) {
        skills::add(
            c,
            o(vec![
                ("name", jstr(name)),
                ("purpose", jstr(purpose)),
                ("goal", jstr(purpose)),
                ("tags", ja(tags.into_iter().map(jstr).collect())),
                ("kind", jstr("command")),
                ("run", o(vec![("cmd", jstr(cmd)), ("timeout_s", jint(10))])),
            ]),
            false,
        )
        .unwrap();
    }

    #[test]
    fn picks_obvious_match_and_explains() {
        let (base, c) = temp_ctx();
        add_skill(&c, "cargo-test", "run the rust test suite", vec!["test", "rust"], "echo test");
        add_skill(&c, "deploy-site", "deploy the website to production hosting", vec!["deploy"], "echo deploy");
        let sel = select(&c, "please run the rust tests").unwrap();
        assert_eq!(sel.candidates[0].name, "cargo-test");
        let text = render(&sel, 5);
        assert!(text.contains("because:"));
        assert!(text.contains("covers"));
        assert!(text.contains("runner-up: deploy-site"));
        // selection journaled with factors
        let tail = c.journal().tail(3).unwrap();
        let ev = tail.iter().find(|e| e.str_of("kind") == "select.explain").unwrap();
        assert_eq!(ev.get("data").unwrap().str_of("chosen"), "cargo-test");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reliability_separates_equal_text_matches() {
        let (base, c) = temp_ctx();
        add_skill(&c, "fmt-a", "format source code files", vec![], "echo a");
        add_skill(&c, "fmt-b", "format source code files", vec![], "echo b");
        // fmt-a earns a good record; fmt-b earns a bad one
        for _ in 0..4 {
            skills::record_outcome(&c, "fmt-a", true, 5, "ok").unwrap();
            skills::record_outcome(&c, "fmt-b", false, 5, "boom").unwrap();
        }
        let sel = select(&c, "format the code").unwrap();
        assert_eq!(sel.candidates[0].name, "fmt-a");
        let rel = sel.candidates[0]
            .factors
            .iter()
            .find(|f| f.name == "reliability")
            .unwrap();
        assert!(rel.note.contains("4/4"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn lessons_boost_selection() {
        let (base, c) = temp_ctx();
        add_skill(&c, "backup-db", "create a database backup archive", vec![], "echo backup");
        add_skill(&c, "dump-db", "create a database backup archive", vec![], "echo dump");
        crate::knowledge::add(
            &c,
            "lesson",
            "database backup works best with backup-db",
            "for database backup tasks the backup-db skill handled permissions correctly",
            &["database".into(), "backup".into()],
        )
        .unwrap();
        let sel = select(&c, "make a database backup").unwrap();
        assert_eq!(sel.candidates[0].name, "backup-db");
        let kn = sel.candidates[0].factors.iter().find(|f| f.name == "knowledge").unwrap();
        assert!(kn.value > 0.0);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn empty_inputs_error_clearly() {
        let (base, c) = temp_ctx();
        assert!(select(&c, "   ").is_err());
        assert!(select(&c, "do something").unwrap_err().contains("no skills"));
        std::fs::remove_dir_all(&base).ok();
    }
}

#[cfg(test)]
mod rerank_tests {
    use super::*;
    use crate::json::{jobj as o, jstr as s};
    use crate::project::testutil::temp_ctx;

    #[test]
    fn echo_rerank_keeps_or_promotes_named_candidate() {
        let (base, mut c) = temp_ctx();
        // route everything to echo: the echoed prompt contains all candidate
        // names, so the first match (heuristic winner) is "picked" — agreement
        let route = |m: &str| o(vec![("provider", s("echo")), ("model", s(m))]);
        let mut model = c.config.get("model").cloned().unwrap();
        model.set("routing", o(vec![("simple", route("s")), ("moderate", route("m")), ("complex", route("x"))]));
        c.config.set("model", model);
        c.save_config().unwrap();
        let c = crate::project::Ctx::load(Some(&c.root.to_string_lossy())).unwrap();
        for (name, purpose) in [("fmt-code", "format source code files"), ("lint-code", "lint source code files")] {
            crate::skills::add(&c, o(vec![
                ("name", s(name)),
                ("purpose", s(purpose)),
                ("goal", s(purpose)),
                ("kind", s("command")),
                ("run", o(vec![("cmd", s("echo x")), ("timeout_s", crate::json::jint(5))])),
            ]), false).unwrap();
        }
        let mut sel = select(&c, "format the source code").unwrap();
        let winner = sel.candidates[0].name.clone();
        let note = rerank_with_model(&c, &mut sel).unwrap();
        assert!(note.contains("re-rank"), "{note}");
        assert_eq!(sel.candidates[0].name, winner); // echo agrees with winner
        // journaled
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "select.rerank"));
        std::fs::remove_dir_all(&base).ok();
    }
}
