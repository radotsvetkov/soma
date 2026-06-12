//! R11 — content-addressed response cache with LRU eviction.
//!
//! Key = SHA-256(provider + model + prompt). One file per entry under
//! `.soma/cache/`, a byte cap from config, least-recently-hit eviction.
//! Identical questions stop costing money/compute — and the hit is journaled,
//! so the savings are visible in the audit trail.

use crate::json::{jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::sha256::sha256_hex;
use crate::util::*;
use std::path::PathBuf;

pub fn cache_key(provider: &str, model: &str, prompt: &str) -> String {
    sha256_hex(format!("{provider}\n{model}\n{prompt}").as_bytes())
}

fn entry_path(c: &Ctx, key: &str) -> PathBuf {
    c.cache_dir().join(format!("{key}.json"))
}

pub fn enabled(c: &Ctx) -> bool {
    c.config
        .get("cache")
        .map(|j| j.b_of("enabled"))
        .unwrap_or(true)
}

pub fn max_bytes(c: &Ctx) -> i64 {
    let b = c.config.get("cache").map(|j| j.i_of("max_bytes")).unwrap_or(0);
    if b > 0 {
        b
    } else {
        50 * 1024 * 1024
    }
}

/// Cache lookup; on hit, bumps hit count + last-hit time.
pub fn get(c: &Ctx, key: &str) -> Option<String> {
    let path = entry_path(c, key);
    let mut entry = crate::json::parse(&read_to_string(&path).ok()?).ok()?;
    let reply = entry.str_of("reply");
    if reply.is_empty() {
        return None;
    }
    entry.set("hits", jint(entry.i_of("hits") + 1));
    entry.set("last_hit_ms", jint(now_ms()));
    atomic_write(&path, entry.to_string().as_bytes()).ok()?;
    Some(reply)
}

pub fn put(c: &Ctx, key: &str, provider: &str, model: &str, prompt: &str, reply: &str) -> R<()> {
    let entry = jobj(vec![
        ("created_ms", jint(now_ms())),
        ("last_hit_ms", jint(now_ms())),
        ("hits", jint(0)),
        ("provider", jstr(provider)),
        ("model", jstr(model)),
        ("prompt_sha", jstr(&sha256_hex(prompt.as_bytes()))),
        ("prompt_chars", jint(prompt.chars().count() as i64)),
        ("reply", jstr(reply)),
    ]);
    atomic_write(&entry_path(c, key), entry.to_string().as_bytes())?;
    evict_to_cap(c)
}

/// Evict least-recently-hit entries until the directory fits the byte cap.
fn evict_to_cap(c: &Ctx) -> R<()> {
    let cap = max_bytes(c);
    let mut entries: Vec<(i64, u64, PathBuf)> = Vec::new(); // (last_hit, bytes, path)
    let Ok(rd) = std::fs::read_dir(c.cache_dir()) else {
        return Ok(());
    };
    let mut total: u64 = 0;
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().map(|x| x == "json").unwrap_or(false) {
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            total += bytes;
            let last_hit = read_to_string(&path)
                .ok()
                .and_then(|s| crate::json::parse(&s).ok())
                .map(|j| j.i_of("last_hit_ms"))
                .unwrap_or(0);
            entries.push((last_hit, bytes, path));
        }
    }
    if (total as i64) <= cap {
        return Ok(());
    }
    entries.sort_by_key(|(last_hit, _, _)| *last_hit);
    for (_, bytes, path) in entries {
        if (total as i64) <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(bytes);
        }
    }
    Ok(())
}

pub fn stats(c: &Ctx) -> Json {
    let mut entries = 0i64;
    let mut bytes = 0i64;
    let mut hits = 0i64;
    if let Ok(rd) = std::fs::read_dir(c.cache_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.extension().map(|x| x == "json").unwrap_or(false) {
                entries += 1;
                bytes += e.metadata().map(|m| m.len() as i64).unwrap_or(0);
                if let Ok(s) = read_to_string(&path) {
                    if let Ok(j) = crate::json::parse(&s) {
                        hits += j.i_of("hits");
                    }
                }
            }
        }
    }
    jobj(vec![
        ("enabled", crate::json::jbool(enabled(c))),
        ("entries", jint(entries)),
        ("bytes", jint(bytes)),
        ("max_bytes", jint(max_bytes(c))),
        ("hits_total", jint(hits)),
    ])
}

pub fn clear(c: &Ctx) -> R<usize> {
    let mut removed = 0;
    if let Ok(rd) = std::fs::read_dir(c.cache_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.extension().map(|x| x == "json").unwrap_or(false)
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    c.log("cache.clear", jobj(vec![("removed", jint(removed as i64))]))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    #[test]
    fn put_get_hit_counting() {
        let (base, c) = temp_ctx();
        let key = cache_key("echo", "m", "what is rust?");
        assert!(get(&c, &key).is_none());
        put(&c, &key, "echo", "m", "what is rust?", "a language").unwrap();
        assert_eq!(get(&c, &key).unwrap(), "a language");
        assert_eq!(get(&c, &key).unwrap(), "a language");
        let s = stats(&c);
        assert_eq!(s.i_of("entries"), 1);
        assert_eq!(s.i_of("hits_total"), 2);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keys_differ_by_provider_model_prompt() {
        let a = cache_key("ollama", "llama3.2", "hi");
        assert_ne!(a, cache_key("anthropic", "llama3.2", "hi"));
        assert_ne!(a, cache_key("ollama", "other", "hi"));
        assert_ne!(a, cache_key("ollama", "llama3.2", "hi!"));
    }

    #[test]
    fn lru_eviction_respects_cap() {
        let (base, mut c) = temp_ctx();
        let mut cache_cfg = c.config.get("cache").cloned().unwrap();
        cache_cfg.set("max_bytes", jint(900)); // ~2 entries worth
        c.config.set("cache", cache_cfg);
        c.save_config().unwrap();

        let filler = "x".repeat(200);
        let k1 = cache_key("e", "m", "one");
        let k2 = cache_key("e", "m", "two");
        put(&c, &k1, "e", "m", "one", &filler).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        put(&c, &k2, "e", "m", "two", &filler).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _ = get(&c, &k2); // k2 most recently hit
        let k3 = cache_key("e", "m", "three");
        put(&c, &k3, "e", "m", "three", &filler).unwrap(); // forces eviction
        assert!(get(&c, &k1).is_none(), "least-recently-hit should be evicted");
        assert!(get(&c, &k2).is_some());
        assert!(get(&c, &k3).is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn clear_removes_everything() {
        let (base, c) = temp_ctx();
        for i in 0..3 {
            let k = cache_key("e", "m", &format!("p{i}"));
            put(&c, &k, "e", "m", &format!("p{i}"), "r").unwrap();
        }
        assert_eq!(clear(&c).unwrap(), 3);
        assert_eq!(stats(&c).i_of("entries"), 0);
        std::fs::remove_dir_all(&base).ok();
    }
}
