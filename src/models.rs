//! R9/R10 — model providers and the hybrid router.
//!
//! Three providers:
//!   echo       deterministic offline provider for tests and dry runs
//!   ollama     local models over plain HTTP (no TLS stack needed)
//!   anthropic  cloud, via the system `curl` (OS owns TLS; key from env,
//!              passed in a 0600 header file — never on the command line)
//!
//! The router classifies a task simple|moderate|complex with named factors,
//! maps it through the routing table in config (difficult → big cloud model,
//! simple → small local model), probes availability, and falls back with the
//! reason journaled. Every call and every routing decision is an event.

use crate::cache;
use crate::json::{jbool, jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::sha256::sha256_hex;
use crate::util::*;
use std::io::Write as _;
use std::time::Instant;

pub const PROVIDERS: [&str; 3] = ["echo", "ollama", "anthropic"];

#[derive(Debug)]
pub struct ModelReply {
    pub text: String,
    pub provider: String,
    pub model: String,
    pub ms: i64,
    pub cached: bool,
}

// ---------- provider config helpers ----------

fn ollama_url(c: &Ctx) -> String {
    let u = c
        .config
        .get("model")
        .map(|m| m.str_of("ollama_url"))
        .unwrap_or_default();
    if u.is_empty() {
        "http://127.0.0.1:11434".into()
    } else {
        u
    }
}

fn max_tokens(c: &Ctx) -> i64 {
    let t = c.config.get("model").map(|m| m.i_of("max_tokens")).unwrap_or(0);
    if t > 0 {
        t
    } else {
        1024
    }
}

fn host_of(url: &str) -> String {
    crate::http::parse_url(url)
        .map(|(h, _, _)| h)
        .unwrap_or_else(|_| "unknown".into())
}

// ---------- availability (R10 fallback) ----------

/// Is a provider usable right now? Err carries the human-readable reason.
pub fn provider_ready(c: &Ctx, provider: &str) -> Result<(), String> {
    match provider {
        "echo" => Ok(()),
        "ollama" => {
            let url = ollama_url(c);
            let host = host_of(&url);
            let dec = c.policy.check_network(&host);
            if !dec.allowed() {
                return Err(format!("policy blocks network to {host} ({})", dec.rule()));
            }
            match crate::http::get(&format!("{url}/api/tags"), 2) {
                Ok(r) if r.status == 200 => Ok(()),
                Ok(r) => Err(format!("ollama at {url} answered HTTP {}", r.status)),
                Err(e) => Err(format!("ollama unreachable at {url} ({e})")),
            }
        }
        "anthropic" => {
            let dec = c.policy.check_network("api.anthropic.com");
            if !dec.allowed() {
                return Err(format!("policy blocks network ({})", dec.rule()));
            }
            if std::env::var("ANTHROPIC_API_KEY").unwrap_or_default().is_empty() {
                return Err("ANTHROPIC_API_KEY is not set".into());
            }
            Ok(())
        }
        other => Err(format!("unknown provider '{other}'")),
    }
}

// ---------- providers (R9) ----------

fn ask_echo(model: &str, prompt: &str) -> R<String> {
    Ok(format!("[echo:{model}] {prompt}"))
}

fn ask_ollama(c: &Ctx, model: &str, prompt: &str) -> R<String> {
    let url = ollama_url(c);
    let body = jobj(vec![
        ("model", jstr(model)),
        ("prompt", jstr(prompt)),
        ("stream", jbool(false)),
        (
            "options",
            jobj(vec![("num_predict", jint(max_tokens(c)))]),
        ),
    ]);
    let resp = crate::http::post_json(&format!("{url}/api/generate"), &body.to_string(), 120)?;
    let j = crate::json::parse(&resp.body)
        .map_err(|e| format!("ollama returned non-json (HTTP {}): {e}", resp.status))?;
    let err = j.str_of("error");
    if !err.is_empty() {
        return Err(format!("ollama: {err}"));
    }
    let text = j.str_of("response");
    if text.is_empty() && resp.status != 200 {
        return Err(format!("ollama HTTP {}", resp.status));
    }
    Ok(text)
}

fn ask_anthropic(c: &Ctx, model: &str, prompt: &str) -> R<String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY is not set".to_string())?;
    let body = jobj(vec![
        ("model", jstr(model)),
        ("max_tokens", jint(max_tokens(c))),
        (
            "messages",
            crate::json::jarr(vec![jobj(vec![
                ("role", jstr("user")),
                ("content", jstr(prompt)),
            ])]),
        ),
    ]);

    // Headers go in a 0600 temp file (`curl -H @file`) so the API key never
    // appears in the process list; body goes via stdin.
    let hdr_path = std::env::temp_dir().join(format!("{}.hdr", new_id("soma")));
    {
        use std::os::unix::fs::PermissionsExt;
        ctx(
            std::fs::write(
                &hdr_path,
                format!(
                    "x-api-key: {key}\nanthropic-version: 2023-06-01\ncontent-type: application/json\n"
                ),
            ),
            "write curl header file",
        )?;
        let _ = std::fs::set_permissions(&hdr_path, std::fs::Permissions::from_mode(0o600));
    }
    let result = (|| -> R<String> {
        let mut child = ctx(
            std::process::Command::new("curl")
                .args([
                    "-sS",
                    "--max-time",
                    "120",
                    "-H",
                    &format!("@{}", hdr_path.display()),
                    "--data-binary",
                    "@-",
                    "https://api.anthropic.com/v1/messages",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn(),
            "spawn curl",
        )?;
        child
            .stdin
            .take()
            .ok_or("curl stdin unavailable")?
            .write_all(body.to_string().as_bytes())
            .map_err(|e| format!("send request body: {e}"))?;
        let out = ctx(child.wait_with_output(), "curl")?;
        if !out.status.success() {
            return Err(format!(
                "curl failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let raw = String::from_utf8_lossy(&out.stdout).to_string();
        let j = crate::json::parse(&raw).map_err(|e| format!("anthropic returned non-json: {e}"))?;
        if j.str_of("type") == "error" {
            let msg = j
                .get("error")
                .map(|e| e.str_of("message"))
                .unwrap_or_default();
            return Err(format!("anthropic api error: {msg}"));
        }
        let text: String = j
            .arr_of("content")
            .iter()
            .filter(|b| b.str_of("type") == "text")
            .map(|b| b.str_of("text"))
            .collect::<Vec<_>>()
            .join("");
        if text.is_empty() {
            return Err("anthropic reply had no text content".into());
        }
        Ok(text)
    })();
    let _ = std::fs::remove_file(&hdr_path);
    result
}

/// Enforce AND journal the network policy for a provider before any egress.
/// This is the single choke point every real provider call passes through —
/// the routed path and the direct `model ask --provider X` path both reach it
/// via `ask()`, so the gate cannot be bypassed (R3/R9).
fn gate_network(c: &Ctx, provider: &str) -> R<()> {
    let host = match provider {
        "ollama" => host_of(&ollama_url(c)),
        "anthropic" => "api.anthropic.com".to_string(),
        _ => return Ok(()), // echo: no egress
    };
    let dec = c.policy.check_network(&host);
    c.log("policy.decision", dec.to_json(&format!("network:{host}")))?;
    if !dec.allowed() {
        return Err(format!("network blocked by policy for {host} ({})", dec.rule()));
    }
    Ok(())
}

/// Direct, uncached model call through the policy gate. Journals the call.
pub fn ask(c: &Ctx, provider: &str, model: &str, prompt: &str) -> R<ModelReply> {
    gate_network(c, provider)?;
    let started = Instant::now();
    let text = match provider {
        "echo" => ask_echo(model, prompt),
        "ollama" => ask_ollama(c, model, prompt),
        "anthropic" => ask_anthropic(c, model, prompt),
        other => Err(format!("unknown provider '{other}' (have: {PROVIDERS:?})")),
    };
    let ms = started.elapsed().as_millis() as i64;
    let ok = text.is_ok();
    c.log(
        "model.call",
        jobj(vec![
            ("provider", jstr(provider)),
            ("model", jstr(model)),
            ("prompt_sha", jstr(&sha256_hex(prompt.as_bytes()))),
            ("prompt_excerpt", jstr(&truncate_chars(prompt, 80))),
            ("prompt_chars", jint(prompt.chars().count() as i64)),
            (
                "reply_chars",
                jint(text.as_ref().map(|t| t.chars().count() as i64).unwrap_or(0)),
            ),
            ("ms", jint(ms)),
            ("ok", jbool(ok)),
            ("cached", jbool(false)),
            (
                "error",
                jstr(text.as_ref().err().map(|e| e.as_str()).unwrap_or("")),
            ),
        ]),
    )?;
    Ok(ModelReply {
        text: text?,
        provider: provider.into(),
        model: model.into(),
        ms,
        cached: false,
    })
}

/// Cache-aware call (R11): hit → journaled cache hit, no provider traffic.
pub fn ask_cached(c: &Ctx, provider: &str, model: &str, prompt: &str) -> R<ModelReply> {
    if cache::enabled(c) {
        // Fold generation params into the key so a different max_tokens (sent
        // to the provider) doesn't collide with a prior reply (R11).
        let keyed = format!("max_tokens={}\n{}", max_tokens(c), prompt);
        let key = cache::cache_key(provider, model, &keyed);
        if let Some(text) = cache::get(c, &key) {
            c.log(
                "model.call",
                jobj(vec![
                    ("provider", jstr(provider)),
                    ("model", jstr(model)),
                    ("prompt_sha", jstr(&sha256_hex(prompt.as_bytes()))),
                    ("prompt_chars", jint(prompt.chars().count() as i64)),
                    ("reply_chars", jint(text.chars().count() as i64)),
                    ("ms", jint(0)),
                    ("ok", jbool(true)),
                    ("cached", jbool(true)),
                ]),
            )?;
            return Ok(ModelReply {
                text,
                provider: provider.into(),
                model: model.into(),
                ms: 0,
                cached: true,
            });
        }
        let reply = ask(c, provider, model, prompt)?;
        cache::put(c, &key, provider, model, prompt, &reply.text)?;
        return Ok(reply);
    }
    ask(c, provider, model, prompt)
}

// ---------- difficulty classifier + router (R10) ----------

#[derive(Debug)]
pub struct Route {
    pub level: String,
    pub points: i64,
    pub factors: Vec<(String, i64)>,
    pub provider: String,
    pub model: String,
    pub fallback_from: Option<String>,
}

const DESIGN_WORDS: [&str; 12] = [
    "design", "architect", "refactor", "optimize", "research", "analyze",
    "strategy", "plan", "spec", "implement", "investigate", "prototype",
];
const CODE_MARKERS: [&str; 10] = [
    "```", "fn ", "def ", "class ", "error", "stack trace", "compile", "bug",
    "panic", "exception",
];
const STEP_WORDS: [&str; 9] =
    ["then", "after", "first", "second", "finally", "step", "steps", "stage", "workflow"];
const SIMPLE_VERBS: [&str; 8] = ["list", "show", "print", "echo", "count", "rename", "what", "status"];

pub fn classify(task: &str) -> (String, i64, Vec<(String, i64)>) {
    let lower = task.to_lowercase();
    let tokens = tokenize(&lower);
    let mut factors: Vec<(String, i64)> = Vec::new();
    let chars = task.chars().count();
    if chars > 600 {
        factors.push(("very long task (>600 chars)".into(), 2));
    } else if chars > 200 {
        factors.push(("long task (>200 chars)".into(), 1));
    }
    let steps = STEP_WORDS.iter().filter(|w| tokens.contains(&w.to_string())).count();
    if steps >= 2 {
        factors.push((format!("multi-step language ({steps} markers)"), 1));
    }
    let design: Vec<&str> = DESIGN_WORDS
        .iter()
        .filter(|w| tokens.contains(&w.to_string()))
        .copied()
        .collect();
    if !design.is_empty() {
        // one design word suggests engineering work; several suggest a project
        let pts = if design.len() >= 3 { 3 } else { 2 };
        factors.push((format!("design/engineering vocabulary ({})", design.join(", ")), pts));
    }
    if CODE_MARKERS.iter().any(|m| lower.contains(m)) {
        factors.push(("code/debugging markers".into(), 1));
    }
    let simple = SIMPLE_VERBS.iter().any(|w| tokens.first().map(|t| t == w).unwrap_or(false));
    if simple && chars < 80 {
        factors.push(("short imperative lookup".into(), -1));
    }
    let points: i64 = factors.iter().map(|(_, p)| p).sum();
    let level = if points <= 0 {
        "simple"
    } else if points <= 2 {
        "moderate"
    } else {
        "complex"
    };
    (level.to_string(), points, factors)
}

fn routing_for(c: &Ctx, level: &str) -> (String, String) {
    let r = c
        .config
        .get("model")
        .and_then(|m| m.get("routing"))
        .and_then(|r| r.get(level))
        .cloned()
        .unwrap_or_else(|| jobj(vec![]));
    let provider = r.str_of("provider");
    let model = r.str_of("model");
    if provider.is_empty() {
        ("echo".into(), "none".into())
    } else {
        (provider, model)
    }
}

/// Classify, map through the routing table, probe availability, fall back
/// in order of escalating capability — and journal the whole decision.
pub fn route(c: &Ctx, task: &str) -> R<Route> {
    let (level, points, factors) = classify(task);
    let (provider, model) = routing_for(c, &level);

    let mut fallback_from = None;
    let (provider, model) = match provider_ready(c, &provider) {
        Ok(()) => (provider, model),
        Err(reason) => {
            // try the other tiers in order: same → moderate → complex → simple
            let mut alternatives: Vec<(String, String)> = ["moderate", "complex", "simple"]
                .iter()
                .filter(|l| **l != level)
                .map(|l| routing_for(c, l))
                .collect();
            alternatives.dedup();
            let mut chosen: Option<(String, String)> = None;
            for (p, m) in alternatives {
                if p != provider && provider_ready(c, &p).is_ok() {
                    chosen = Some((p, m));
                    break;
                }
            }
            let (p, m) = chosen.ok_or_else(|| {
                format!(
                    "no model provider available — {provider} ({reason}), and no fallback tier is ready\n  hints: start a local model (`ollama serve`, then `ollama pull llama3.2`)\n         or opt into cloud: `soma preset apply hybrid-default` + export ANTHROPIC_API_KEY"
                )
            })?;
            fallback_from = Some(format!("{provider}: {reason}"));
            (p, m)
        }
    };

    c.log(
        "model.route",
        jobj(vec![
            ("task_excerpt", jstr(&truncate_chars(task, 100))),
            ("level", jstr(&level)),
            ("points", jint(points)),
            (
                "factors",
                crate::json::jarr(
                    factors
                        .iter()
                        .map(|(n, p)| jobj(vec![("factor", jstr(n)), ("points", jint(*p))]))
                        .collect(),
                ),
            ),
            ("provider", jstr(&provider)),
            ("model", jstr(&model)),
            (
                "fallback_from",
                fallback_from
                    .as_ref()
                    .map(|s| jstr(s))
                    .unwrap_or(Json::Null),
            ),
        ]),
    )?;
    Ok(Route {
        level,
        points,
        factors,
        provider,
        model,
        fallback_from,
    })
}

/// Route a task and ask the chosen model (cache-aware).
pub fn ask_routed(c: &Ctx, task: &str) -> R<(Route, ModelReply)> {
    let r = route(c, task)?;
    let reply = ask_cached(c, &r.provider, &r.model, task)?;
    Ok((r, reply))
}

pub fn render_route(r: &Route) -> String {
    let mut out = format!(
        "difficulty: {} ({} points)\n",
        r.level, r.points
    );
    for (name, pts) in &r.factors {
        out.push_str(&format!("    {pts:+}  {name}\n"));
    }
    if r.factors.is_empty() {
        out.push_str("    (no complexity signals — defaults to simple)\n");
    }
    out.push_str(&format!("→ provider: {} model: {}\n", r.provider, r.model));
    if let Some(f) = &r.fallback_from {
        out.push_str(&format!("  (fell back from {f})\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    fn set_routing(c: &mut Ctx, simple: (&str, &str), moderate: (&str, &str), complex: (&str, &str)) {
        let route = |p: &str, m: &str| jobj(vec![("provider", jstr(p)), ("model", jstr(m))]);
        let mut model = c.config.get("model").cloned().unwrap();
        model.set(
            "routing",
            jobj(vec![
                ("simple", route(simple.0, simple.1)),
                ("moderate", route(moderate.0, moderate.1)),
                ("complex", route(complex.0, complex.1)),
            ]),
        );
        c.config.set("model", model);
        c.save_config().unwrap();
    }

    #[test]
    fn classifier_levels() {
        let (lvl, _, _) = classify("list files in the project");
        assert_eq!(lvl, "simple");
        let (lvl, _, factors) = classify(
            "design and architect a refactor of the storage engine, then implement it step by step, \
             then optimize the hot path and analyze memory usage with a detailed plan",
        );
        assert_eq!(lvl, "complex");
        assert!(!factors.is_empty());
        let (lvl, _, _) = classify("fix the bug in the parser");
        assert_eq!(lvl, "moderate");
    }

    #[test]
    fn echo_ask_and_cache_hit() {
        let (base, c) = temp_ctx();
        let r1 = ask_cached(&c, "echo", "m1", "what is soma?").unwrap();
        assert!(!r1.cached);
        assert!(r1.text.contains("what is soma?"));
        let r2 = ask_cached(&c, "echo", "m1", "what is soma?").unwrap();
        assert!(r2.cached);
        assert_eq!(r1.text, r2.text);
        // both calls journaled, second as cache hit
        let tail = c.journal().tail(10).unwrap();
        let calls: Vec<&Json> = tail
            .iter()
            .filter(|e| e.str_of("kind") == "model.call")
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].get("data").unwrap().b_of("cached"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn route_uses_config_and_falls_back_with_reason() {
        let (base, mut c) = temp_ctx();
        // point ollama somewhere dead so it's unavailable
        let mut model = c.config.get("model").cloned().unwrap();
        model.set("ollama_url", jstr("http://127.0.0.1:1"));
        c.config.set("model", model);
        set_routing(&mut c, ("ollama", "tiny"), ("echo", "mid"), ("echo", "big"));
        let r = route(&c, "list files").unwrap();
        assert_eq!(r.level, "simple");
        assert_eq!(r.provider, "echo", "should fall back to a ready tier");
        assert!(r.fallback_from.as_ref().unwrap().contains("ollama"));
        // journaled with fallback reason
        let tail = c.journal().tail(5).unwrap();
        let ev = tail.iter().find(|e| e.str_of("kind") == "model.route").unwrap();
        assert!(ev.get("data").unwrap().str_of("fallback_from").contains("ollama"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn route_errors_when_nothing_ready() {
        let (base, mut c) = temp_ctx();
        let mut model = c.config.get("model").cloned().unwrap();
        model.set("ollama_url", jstr("http://127.0.0.1:1"));
        c.config.set("model", model);
        set_routing(&mut c, ("ollama", "a"), ("ollama", "b"), ("ollama", "c"));
        let err = route(&c, "list files").unwrap_err();
        assert!(err.contains("no model provider available"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn anthropic_requires_network_policy_and_key() {
        let (base, c) = temp_ctx();
        // default policy: allow_network=false → blocked before key check
        let err = provider_ready(&c, "anthropic").unwrap_err();
        assert!(err.contains("policy blocks network"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn direct_ask_cannot_bypass_network_policy() {
        let (base, c) = temp_ctx();
        // allow_network=false by default. A direct `ask` to a cloud host must
        // be refused at the gate BEFORE any egress, and journaled — this is
        // the bypass the verification pass found and we closed.
        let err = ask(&c, "anthropic", "claude-fable-5", "hi").unwrap_err();
        assert!(err.contains("network blocked by policy"), "{err}");
        let tail = c.journal().tail(10).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && e.get("data").unwrap().str_of("subject") == "network:api.anthropic.com"
            && !e.get("data").unwrap().b_of("allowed")));

        // localhost (ollama default 127.0.0.1) is allowed even under
        // local-only — it must pass the gate. Hermetic across environments:
        // with a live local ollama the call succeeds (gate passed); without
        // one it fails on *connection* — never on policy.
        match ask(&c, "ollama", "llama3.2", "hi") {
            Ok(_) => {} // live local model answered — gate clearly passed
            Err(err2) => {
                assert!(!err2.contains("blocked by policy"), "localhost should pass the gate: {err2}")
            }
        }

        // echo never touches the network → works under local-only.
        assert!(ask(&c, "echo", "m", "hi").is_ok());

        // open the network → anthropic passes the gate (then fails on missing key)
        let mut open = crate::project::Ctx::load(Some(&c.root.to_string_lossy())).unwrap();
        open.policy.allow_network = true;
        open.save_policy().unwrap();
        let err3 = ask(&open, "anthropic", "claude-fable-5", "hi").unwrap_err();
        assert!(!err3.contains("blocked by policy"), "{err3}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn render_route_mentions_fallback() {
        let r = Route {
            level: "simple".into(),
            points: 0,
            factors: vec![],
            provider: "echo".into(),
            model: "m".into(),
            fallback_from: Some("ollama: unreachable".into()),
        };
        let s = render_route(&r);
        assert!(s.contains("fell back from ollama"));
    }
}
