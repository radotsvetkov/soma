//! R17 — projects, the runtime context, and builtin presets.
//!
//! A project is any directory with a `.soma/` inside. `$SOMA_HOME`
//! (default `~/.soma`) holds the global skill registry and the project index.

use crate::events::Journal;
use crate::json::{jint, jobj, jstr, Json};
use crate::policy::Policy;
use crate::util::*;
use std::path::{Path, PathBuf};

pub const SOMA_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UI_API: u32 = 1;

pub fn soma_home() -> PathBuf {
    std::env::var("SOMA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_home("~/.soma"))
}

/// Walk up from `start` looking for a `.soma/` directory.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    while let Some(dir) = cur {
        if dir.join(".soma").is_dir() {
            return Some(dir);
        }
        cur = dir.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Everything a command needs: paths + loaded config + loaded policy.
pub struct Ctx {
    pub home: PathBuf,
    pub root: PathBuf,
    pub dir: PathBuf, // <root>/.soma
    pub config: Json,
    pub policy: Policy,
}

impl Ctx {
    pub fn load(explicit_root: Option<&str>) -> R<Ctx> {
        let root = match explicit_root {
            Some(r) => {
                let p = expand_home(r);
                if !p.join(".soma").is_dir() {
                    return Err(format!("{} is not a soma project (no .soma/)", p.display()));
                }
                p
            }
            None => {
                let cwd = ctx(std::env::current_dir(), "cwd")?;
                find_project_root(&cwd).ok_or_else(|| {
                    "not inside a soma project — run `soma init` first (or pass --project <dir>)"
                        .to_string()
                })?
            }
        };
        let dir = root.join(".soma");
        let config = read_to_string(&dir.join("config.json"))
            .ok()
            .and_then(|s| crate::json::parse(&s).ok())
            .unwrap_or_else(|| default_config("unnamed"));
        let policy = Policy::load(&dir.join("policy.json"))?;
        Ok(Ctx {
            home: soma_home(),
            root,
            dir,
            config,
            policy,
        })
    }

    pub fn name(&self) -> String {
        let n = self.config.str_of("project");
        if n.is_empty() {
            self.root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unnamed".into())
        } else {
            n
        }
    }

    pub fn journal(&self) -> Journal {
        Journal::new(&self.dir, self.policy.redact_keys.clone())
    }

    /// Journal an event under this project's redaction policy.
    pub fn log(&self, kind: &str, data: Json) -> R<Json> {
        self.journal().append(kind, data)
    }

    pub fn save_config(&self) -> R<()> {
        atomic_write(
            &self.dir.join("config.json"),
            self.config.pretty().as_bytes(),
        )
    }

    pub fn save_policy(&self) -> R<()> {
        self.policy.save(&self.dir.join("policy.json"))
    }

    // Well-known paths.
    pub fn skills_dir(&self) -> PathBuf {
        self.dir.join("skills")
    }
    pub fn global_skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }
    pub fn metrics_path(&self) -> PathBuf {
        self.dir.join("metrics.json")
    }
    pub fn issues_path(&self) -> PathBuf {
        self.dir.join("issues.jsonl")
    }
    pub fn proposals_path(&self) -> PathBuf {
        self.dir.join("proposals.jsonl")
    }
    pub fn knowledge_path(&self) -> PathBuf {
        self.dir.join("knowledge.jsonl")
    }
    pub fn goals_path(&self) -> PathBuf {
        self.dir.join("goals.jsonl")
    }
    pub fn crons_path(&self) -> PathBuf {
        self.dir.join("crons.json")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.dir.join("cache")
    }
    pub fn mcp_path(&self) -> PathBuf {
        self.dir.join("mcp.json")
    }
}

/// Fresh projects start fully local (zero egress) — the secure default.
/// `soma preset apply hybrid-default` opts into cloud routing explicitly.
pub fn default_config(name: &str) -> Json {
    jobj(vec![
        ("project", jstr(name)),
        ("created", jstr(&iso8601(now_ms()))),
        ("preset", jstr("local-only")),
        (
            "model",
            jobj(vec![
                (
                    "routing",
                    jobj(vec![
                        (
                            "simple",
                            jobj(vec![("provider", jstr("ollama")), ("model", jstr("llama3.2:1b"))]),
                        ),
                        (
                            "moderate",
                            jobj(vec![("provider", jstr("ollama")), ("model", jstr("llama3.2"))]),
                        ),
                        (
                            "complex",
                            jobj(vec![("provider", jstr("ollama")), ("model", jstr("qwen2.5:14b"))]),
                        ),
                    ]),
                ),
                ("ollama_url", jstr("http://127.0.0.1:11434")),
                ("max_tokens", jint(1024)),
            ]),
        ),
        (
            "cache",
            jobj(vec![("enabled", crate::json::jbool(true)), ("max_bytes", jint(50 * 1024 * 1024))]),
        ),
        (
            "anchor",
            jobj(vec![
                ("tsa_url", jstr(crate::anchor::DEFAULT_TSA_URL)),
                ("auto", jstr("off")),
            ]),
        ),
    ])
}

/// Create `.soma/` in `dir`, register the project globally, journal it.
pub fn init(dir: &Path, name: Option<&str>) -> R<Ctx> {
    let root = ctx(dir.canonicalize(), "canonicalize project dir")?;
    let sdir = root.join(".soma");
    if sdir.is_dir() {
        return Err(format!("{} is already a soma project", root.display()));
    }
    let name = name
        .map(|s| s.to_string())
        .or_else(|| root.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "project".into());
    ensure_dir(&sdir)?;
    ensure_dir(&sdir.join("skills"))?;
    ensure_dir(&sdir.join("cache"))?;
    atomic_write(
        &sdir.join("config.json"),
        default_config(&name).pretty().as_bytes(),
    )?;
    Policy::default_policy().save(&sdir.join("policy.json"))?;

    let home = soma_home();
    ensure_dir(&home.join("skills"))?;
    register_project(&home, &name, &root)?;

    let c = Ctx::load(Some(&root.to_string_lossy()))?;
    c.log(
        "project.init",
        jobj(vec![
            ("name", jstr(&name)),
            ("root", jstr(root.to_string_lossy().as_ref())),
            ("soma_version", jstr(SOMA_VERSION)),
        ]),
    )?;
    Ok(c)
}

fn register_project(home: &Path, name: &str, root: &Path) -> R<()> {
    let path = home.join("projects.json");
    let mut list = read_to_string(&path)
        .ok()
        .and_then(|s| crate::json::parse(&s).ok())
        .and_then(|j| j.arr().cloned())
        .unwrap_or_default();
    let root_s = root.to_string_lossy().to_string();
    list.retain(|p| p.str_of("root") != root_s);
    list.push(jobj(vec![
        ("name", jstr(name)),
        ("root", jstr(&root_s)),
        ("registered", jstr(&iso8601(now_ms()))),
    ]));
    atomic_write(&path, Json::Arr(list).pretty().as_bytes())
}

pub fn list_projects(home: &Path) -> Vec<Json> {
    read_to_string(&home.join("projects.json"))
        .ok()
        .and_then(|s| crate::json::parse(&s).ok())
        .and_then(|j| j.arr().cloned())
        .unwrap_or_default()
}

// ---------- presets (R17) ----------

pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
}

pub fn presets() -> Vec<Preset> {
    vec![
        Preset {
            name: "hybrid-default",
            description: "(alias: hybrid) simple→local Ollama, moderate→Haiku, complex→Fable; network on (+ TSA hosts freetsa.org, timestamp.digicert.com for anchoring); 50MB cache",
        },
        Preset {
            name: "local-only",
            description: "everything routed to local Ollama; outbound network disabled (no TSA hosts — anchoring refuses)",
        },
        Preset {
            name: "cloud-max",
            description: "simple→Haiku, moderate→Sonnet, complex→Fable; for maximum quality (+ TSA hosts for anchoring)",
        },
        Preset {
            name: "low-ram",
            description: "small local models only, 8MB cache, lean settings for constrained machines",
        },
    ]
}

/// Apply a builtin preset: rewrites model routing, cache, and the network
/// policy bit, then journals exactly what changed.
pub fn apply_preset(c: &mut Ctx, name: &str) -> R<String> {
    let route = |p: &str, m: &str| jobj(vec![("provider", jstr(p)), ("model", jstr(m))]);
    // SPEC names this preset "hybrid"; "hybrid-default" is the original alias.
    let name = if name == "hybrid" { "hybrid-default" } else { name };
    let (routing, cache_bytes, network, tsa_hosts) = match name {
        "hybrid-default" => (
            jobj(vec![
                ("simple", route("ollama", "llama3.2")),
                ("moderate", route("anthropic", "claude-haiku-4-5-20251001")),
                ("complex", route("anthropic", "claude-fable-5")),
            ]),
            50 * 1024 * 1024,
            true,
            true,
        ),
        "local-only" => (
            jobj(vec![
                ("simple", route("ollama", "llama3.2:1b")),
                ("moderate", route("ollama", "llama3.2")),
                ("complex", route("ollama", "qwen2.5:14b")),
            ]),
            50 * 1024 * 1024,
            false,
            false,
        ),
        "cloud-max" => (
            jobj(vec![
                ("simple", route("anthropic", "claude-haiku-4-5-20251001")),
                ("moderate", route("anthropic", "claude-sonnet-4-6")),
                ("complex", route("anthropic", "claude-fable-5")),
            ]),
            50 * 1024 * 1024,
            true,
            true,
        ),
        "low-ram" => (
            jobj(vec![
                ("simple", route("ollama", "llama3.2:1b")),
                ("moderate", route("ollama", "llama3.2:1b")),
                ("complex", route("ollama", "llama3.2")),
            ]),
            8 * 1024 * 1024,
            false,
            false,
        ),
        other => {
            return Err(format!(
                "unknown preset '{other}' — available: {}",
                presets().iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
            ))
        }
    };

    let mut model = c
        .config
        .get("model")
        .cloned()
        .unwrap_or_else(|| jobj(vec![]));
    model.set("routing", routing);
    c.config.set("model", model);
    let mut cache = c
        .config
        .get("cache")
        .cloned()
        .unwrap_or_else(|| jobj(vec![]));
    cache.set("max_bytes", jint(cache_bytes));
    c.config.set("cache", cache);
    c.config.set("preset", jstr(name));
    c.policy.allow_network = network;
    // D10: cloud presets allow the TSA hosts so `soma anchor now` works out
    // of the box; local-only/low-ram strip exactly those hosts — local-only
    // must never anchor, even after a hybrid→local-only switch.
    if tsa_hosts {
        for h in crate::anchor::TSA_HOSTS {
            if !c.policy.allow_hosts.iter().any(|x| x == h) {
                c.policy.allow_hosts.push(h.into());
            }
        }
    } else {
        c.policy
            .allow_hosts
            .retain(|h| !crate::anchor::TSA_HOSTS.contains(&h.as_str()));
    }
    c.save_config()?;
    c.save_policy()?;
    c.log(
        "preset.apply",
        jobj(vec![
            ("preset", jstr(name)),
            ("allow_network", crate::json::jbool(network)),
            (
                "allow_hosts",
                Json::Arr(c.policy.allow_hosts.iter().map(|h| jstr(h)).collect()),
            ),
            ("cache_max_bytes", jint(cache_bytes)),
        ]),
    )?;
    Ok(format!(
        "applied preset '{name}' (network={}, cache={}MB)",
        network,
        cache_bytes / (1024 * 1024)
    ))
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Fresh isolated project + home under a temp dir. Tests using this must
    /// pass explicit roots (no reliance on process cwd). SOMA_HOME is process
    /// global, so construction is serialized; tests must only use the paths
    /// captured in the returned Ctx afterwards.
    pub fn temp_ctx() -> (PathBuf, Ctx) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("soma-it-{}", new_id("t")));
        let home = base.join("home");
        let proj = base.join("proj");
        ensure_dir(&proj).unwrap();
        std::env::set_var("SOMA_HOME", &home);
        let c = init(&proj, Some("testproj")).unwrap();
        (base, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_load_roundtrip() {
        let (base, c) = testutil::temp_ctx();
        assert_eq!(c.name(), "testproj");
        assert!(c.dir.join("config.json").is_file());
        assert!(c.dir.join("policy.json").is_file());
        // journal has the init event
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "project.init"));
        // discoverable by walking up from a subdirectory
        let sub = c.root.join("a/b");
        ensure_dir(&sub).unwrap();
        assert_eq!(find_project_root(&sub).unwrap(), c.root);
        // registered globally
        assert!(list_projects(&c.home)
            .iter()
            .any(|p| p.str_of("name") == "testproj"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn preset_apply_changes_config_policy_and_journals() {
        let (base, mut c) = testutil::temp_ctx();
        apply_preset(&mut c, "local-only").unwrap();
        let reloaded = Ctx::load(Some(&c.root.to_string_lossy())).unwrap();
        assert_eq!(reloaded.config.str_of("preset"), "local-only");
        assert!(!reloaded.policy.allow_network);
        let routing = reloaded
            .config
            .get("model")
            .unwrap()
            .get("routing")
            .unwrap()
            .clone();
        assert_eq!(routing.get("complex").unwrap().str_of("provider"), "ollama");
        let tail = reloaded.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "preset.apply"));
        assert!(apply_preset(&mut c, "nope").is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn presets_manage_tsa_hosts() {
        let (base, mut c) = testutil::temp_ctx();
        // fresh local-only default: no TSA hosts
        assert!(!c.policy.allow_hosts.iter().any(|h| h == "freetsa.org"));
        // hybrid-default gains both TSA hosts (journaled at apply)
        apply_preset(&mut c, "hybrid-default").unwrap();
        let p = Ctx::load(Some(&c.root.to_string_lossy())).unwrap().policy;
        assert!(p.allow_hosts.iter().any(|h| h == "freetsa.org"));
        assert!(p.allow_hosts.iter().any(|h| h == "timestamp.digicert.com"));
        let tail = c.journal().tail(3).unwrap();
        let ev = tail.iter().find(|e| e.str_of("kind") == "preset.apply").unwrap();
        assert!(ev.get("data").unwrap().strs_of("allow_hosts").contains(&"freetsa.org".to_string()));
        // switching to local-only strips exactly those hosts again
        apply_preset(&mut c, "local-only").unwrap();
        let p = Ctx::load(Some(&c.root.to_string_lossy())).unwrap().policy;
        assert!(!p.allow_hosts.iter().any(|h| h == "freetsa.org"));
        assert!(!p.allow_hosts.iter().any(|h| h == "timestamp.digicert.com"));
        assert!(p.allow_hosts.iter().any(|h| h == "localhost"), "non-TSA hosts kept");
        // cloud-max gains them too
        apply_preset(&mut c, "cloud-max").unwrap();
        let p = Ctx::load(Some(&c.root.to_string_lossy())).unwrap().policy;
        assert!(p.allow_hosts.iter().any(|h| h == "freetsa.org"));
        assert!(p.allow_hosts.iter().any(|h| h == "timestamp.digicert.com"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn double_init_rejected() {
        let (base, c) = testutil::temp_ctx();
        assert!(init(&c.root, Some("again")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }
}
