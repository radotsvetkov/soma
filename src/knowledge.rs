//! R8 — the knowledge base: lessons, notes, and references that make the
//! selector smarter over time.
//!
//! Storage is an append-only JSONL per project. Lessons are auto-recorded when
//! a skill issue is resolved, closing the loop: failure → issue → fix →
//! lesson → better future selection. The store is intentionally a thin
//! interface; a memora-backed implementation can replace it later without
//! touching callers (they only use add/list/search).

use crate::json::{jarr, jobj, jstr, Json};
use crate::project::Ctx;
use crate::util::*;

pub const KINDS: [&str; 3] = ["lesson", "note", "reference"];

pub fn add(c: &Ctx, kind: &str, title: &str, body: &str, tags: &[String]) -> R<Json> {
    if !KINDS.contains(&kind) {
        return Err(format!("knowledge kind must be one of {KINDS:?}"));
    }
    let ts = now_ms();
    let entry = jobj(vec![
        ("id", jstr(new_id("kn"))),
        ("ts", crate::json::jint(ts)),
        ("iso", jstr(&iso8601(ts))),
        ("kind", jstr(kind)),
        ("title", jstr(title)),
        ("body", jstr(body)),
        ("tags", jarr(tags.iter().map(|t| jstr(t)).collect())),
    ]);
    append_line(&c.knowledge_path(), &entry.to_string())?;
    c.log(
        "knowledge.add",
        jobj(vec![("kind", jstr(kind)), ("title", jstr(title))]),
    )?;
    Ok(entry)
}

pub fn list(c: &Ctx, limit: usize) -> R<Vec<Json>> {
    Ok(tail_lines(&c.knowledge_path(), limit)?
        .iter()
        .filter_map(|l| crate::json::parse(l).ok())
        .collect())
}

/// Token-overlap search: score = |query ∩ entry| / |query|, with a small tag
/// bonus. Deterministic and explainable, like everything else in soma.
pub fn search(c: &Ctx, query: &str, k: usize) -> R<Vec<(f64, Json)>> {
    let q = tokenize(query);
    if q.is_empty() {
        return Ok(vec![]);
    }
    let mut scored: Vec<(f64, Json)> = Vec::new();
    for_each_line(&c.knowledge_path(), |line| {
        if let Ok(e) = crate::json::parse(line) {
            let text = format!(
                "{} {} {}",
                e.str_of("title"),
                e.str_of("body"),
                e.strs_of("tags").join(" ")
            );
            let toks = tokenize(&text);
            let overlap = q.iter().filter(|t| toks.contains(t)).count() as f64;
            let tag_hit = e
                .strs_of("tags")
                .iter()
                .filter(|t| q.contains(&t.to_lowercase()))
                .count() as f64;
            let score = overlap / q.len() as f64 + 0.25 * tag_hit;
            if score > 0.0 {
                scored.push((score, e));
            }
        }
        Ok(())
    })?;
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(scored)
}

/// Lessons relevant to a task that mention a given skill by name — used by
/// the neuro selector (R6) as an explainable scoring boost.
pub fn lessons_mentioning(c: &Ctx, task: &str, skill: &str) -> R<Vec<String>> {
    let hits = search(c, task, 8)?;
    let skill_lower = skill.to_lowercase();
    Ok(hits
        .into_iter()
        .filter(|(score, e)| {
            *score >= 0.3
                && e.str_of("kind") == "lesson"
                && (e.str_of("body").to_lowercase().contains(&skill_lower)
                    || e.str_of("title").to_lowercase().contains(&skill_lower)
                    || e.strs_of("tags").iter().any(|t| t.to_lowercase() == skill_lower))
        })
        .map(|(_, e)| e.str_of("title"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    #[test]
    fn add_search_and_lesson_boost() {
        let (base, c) = temp_ctx();
        add(
            &c,
            "lesson",
            "cargo-test flaky on low memory",
            "when running cargo-test skill on constrained machines, raise timeout to 300s",
            &["cargo".into(), "testing".into()],
        )
        .unwrap();
        add(&c, "note", "ollama lives on port 11434", "default local endpoint", &[]).unwrap();
        assert!(add(&c, "bogus", "x", "y", &[]).is_err());

        let hits = search(&c, "cargo test timeout", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].1.str_of("title").contains("cargo-test"));

        let mentions = lessons_mentioning(&c, "run the cargo test suite", "cargo-test").unwrap();
        assert_eq!(mentions.len(), 1);
        let none = lessons_mentioning(&c, "deploy website", "deploy").unwrap();
        assert!(none.is_empty());

        assert_eq!(list(&c, 10).unwrap().len(), 2);
        std::fs::remove_dir_all(&base).ok();
    }
}
