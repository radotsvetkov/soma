//! R4/R5 — skills: small programs with a declared purpose and goal.
//!
//! A skill is a JSON manifest in `.soma/skills/` (project) or
//! `$SOMA_HOME/skills/` (global; project shadows global by name). Execution
//! goes through the policy gate, updates per-skill metrics, and files an
//! issue automatically when a run fails — the raw material for the
//! improvement engine (R7).

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::policy::Decision;
use crate::project::Ctx;
use crate::util::*;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Skill {
    pub manifest: Json,
    pub path: PathBuf,
    pub scope: &'static str, // "project" | "global"
}

impl Skill {
    pub fn name(&self) -> String {
        self.manifest.str_of("name")
    }
    pub fn kind(&self) -> String {
        let k = self.manifest.str_of("kind");
        if k.is_empty() {
            "command".into()
        } else {
            k
        }
    }
    pub fn purpose(&self) -> String {
        self.manifest.str_of("purpose")
    }
    pub fn goal(&self) -> String {
        self.manifest.str_of("goal")
    }
    pub fn tags(&self) -> Vec<String> {
        self.manifest.strs_of("tags")
    }
    pub fn archived(&self) -> bool {
        self.manifest.b_of("archived")
    }
    pub fn cmd(&self) -> String {
        self.manifest
            .get("run")
            .map(|r| r.str_of("cmd"))
            .unwrap_or_default()
    }
    pub fn timeout_s(&self) -> i64 {
        let t = self
            .manifest
            .get("run")
            .map(|r| r.i_of("timeout_s"))
            .unwrap_or(0);
        if t > 0 {
            t
        } else {
            120
        }
    }
}

/// Manifest validation (R4). Returns problems; empty = valid.
pub fn lint(m: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    let name = m.str_of("name");
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        problems.push("name: required, [a-zA-Z0-9_-] only".into());
    }
    if m.str_of("purpose").len() < 8 {
        problems.push("purpose: required (≥8 chars) — the selector depends on it".into());
    }
    if m.str_of("goal").is_empty() {
        problems.push("goal: required — what outcome does this skill produce?".into());
    }
    match m.str_of("kind").as_str() {
        "" | "command" => {
            if m.get("run").map(|r| r.str_of("cmd")).unwrap_or_default().is_empty() {
                problems.push("run.cmd: required for command skills".into());
            }
        }
        "mcp" => {
            let r = m.get("run");
            if r.map(|r| r.str_of("server")).unwrap_or_default().is_empty()
                || r.map(|r| r.str_of("tool")).unwrap_or_default().is_empty()
            {
                problems.push("run.server and run.tool: required for mcp skills".into());
            }
        }
        other => problems.push(format!("kind: '{other}' not one of command|mcp")),
    }
    if let Some(s) = m.get("success") {
        let k = s.str_of("kind");
        if k != "exit0" && k != "contains" {
            problems.push("success.kind: must be exit0|contains".into());
        }
        if k == "contains" && s.str_of("value").is_empty() {
            problems.push("success.value: required when success.kind=contains".into());
        }
    }
    problems
}

fn read_skill_dir(dir: &PathBuf, scope: &'static str, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(s) = read_to_string(&p) {
                if let Ok(m) = crate::json::parse(&s) {
                    out.push(Skill {
                        manifest: m,
                        path: p,
                        scope,
                    });
                }
            }
        }
    }
}

/// All skills visible to this project; a project skill shadows a global one
/// with the same name.
pub fn load_all(c: &Ctx) -> Vec<Skill> {
    let mut skills = Vec::new();
    read_skill_dir(&c.skills_dir(), "project", &mut skills);
    let mut global = Vec::new();
    read_skill_dir(&c.global_skills_dir(), "global", &mut global);
    for g in global {
        if !skills.iter().any(|s| s.name() == g.name()) {
            skills.push(g);
        }
    }
    skills
}

pub fn find(c: &Ctx, name: &str) -> R<Skill> {
    load_all(c)
        .into_iter()
        .find(|s| s.name() == name)
        .ok_or_else(|| format!("no skill named '{name}' (see `soma skill list`)"))
}

/// Validate + write a manifest into the project (or global) registry.
pub fn add(c: &Ctx, mut manifest: Json, global: bool) -> R<PathBuf> {
    let problems = lint(&manifest);
    if !problems.is_empty() {
        return Err(format!("manifest invalid:\n  - {}", problems.join("\n  - ")));
    }
    if manifest.get("version").is_none() {
        manifest.set("version", jint(1));
    }
    if manifest.get("created").is_none() {
        manifest.set("created", jstr(&iso8601(now_ms())));
    }
    let dir = if global {
        c.global_skills_dir()
    } else {
        c.skills_dir()
    };
    let path = dir.join(format!("{}.json", manifest.str_of("name")));
    atomic_write(&path, manifest.pretty().as_bytes())?;
    c.log(
        "skill.add",
        jobj(vec![
            ("name", jstr(&manifest.str_of("name"))),
            ("scope", jstr(if global { "global" } else { "project" })),
            ("version", jint(manifest.i_of("version"))),
        ]),
    )?;
    Ok(path)
}

// ---------- metrics (R5) ----------

pub fn load_metrics(c: &Ctx) -> Json {
    read_to_string(&c.metrics_path())
        .ok()
        .and_then(|s| crate::json::parse(&s).ok())
        .unwrap_or_else(|| jobj(vec![]))
}

pub fn metrics_for<'a>(metrics: &'a Json, skill: &str) -> Option<&'a Json> {
    metrics.get(skill)
}

/// Record a run outcome: update metrics, journal, and file an issue on
/// failure. Shared by command skills here and MCP skills in mcp.rs.
pub fn record_outcome(c: &Ctx, skill: &str, ok: bool, ms: i64, detail: &str) -> R<()> {
    let mut metrics = load_metrics(c);
    let mut m = metrics
        .get(skill)
        .cloned()
        .unwrap_or_else(|| jobj(vec![]));
    m.set("runs", jint(m.i_of("runs") + 1));
    m.set("ok", jint(m.i_of("ok") + if ok { 1 } else { 0 }));
    m.set("fail", jint(m.i_of("fail") + if ok { 0 } else { 1 }));
    m.set("total_ms", jint(m.i_of("total_ms") + ms));
    m.set("last_used_ms", jint(now_ms()));
    m.set("last_ok", jbool(ok));
    metrics.set(skill, m);
    atomic_write(&c.metrics_path(), metrics.pretty().as_bytes())?;
    c.log(
        "skill.run",
        jobj(vec![
            ("name", jstr(skill)),
            ("ok", jbool(ok)),
            ("ms", jint(ms)),
            ("detail", jstr(&truncate_chars(detail, 200))),
        ]),
    )?;
    if !ok {
        file_issue(c, skill, "run_failure", detail)?;
    }
    Ok(())
}

// ---------- issues (R5) ----------

pub fn file_issue(c: &Ctx, skill: &str, kind: &str, detail: &str) -> R<Json> {
    let ts = now_ms();
    let issue = jobj(vec![
        ("id", jstr(new_id("is"))),
        ("ts", jint(ts)),
        ("iso", jstr(&iso8601(ts))),
        ("skill", jstr(skill)),
        ("kind", jstr(kind)),
        ("detail", jstr(&truncate_chars(detail, 500))),
        ("status", jstr("open")),
    ]);
    append_line(&c.issues_path(), &issue.to_string())?;
    c.log(
        "skill.issue",
        jobj(vec![("skill", jstr(skill)), ("kind", jstr(kind))]),
    )?;
    Ok(issue)
}

pub fn list_issues(c: &Ctx, only_open: bool) -> R<Vec<Json>> {
    let mut out = Vec::new();
    for_each_line(&c.issues_path(), |line| {
        if let Ok(i) = crate::json::parse(line) {
            if !only_open || i.str_of("status") == "open" {
                out.push(i);
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Mark an issue resolved and record the resolution as a knowledge lesson —
/// the self-improvement loop's memory (R8).
pub fn resolve_issue(c: &Ctx, id: &str, note: &str) -> R<()> {
    let mut found: Option<Json> = None;
    let mut lines: Vec<Json> = Vec::new();
    for_each_line(&c.issues_path(), |line| {
        if let Ok(mut i) = crate::json::parse(line) {
            if i.str_of("id") == id {
                i.set("status", jstr("resolved"));
                i.set("resolution", jstr(note));
                i.set("resolved_iso", jstr(&iso8601(now_ms())));
                found = Some(i.clone());
            }
            lines.push(i);
        }
        Ok(())
    })?;
    let issue = found.ok_or_else(|| format!("no issue with id {id}"))?;
    let content = lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    atomic_write(&c.issues_path(), content.as_bytes())?;
    crate::knowledge::add(
        c,
        "lesson",
        &format!("resolved: {} on {}", issue.str_of("kind"), issue.str_of("skill")),
        &format!(
            "issue: {} — resolution: {note}",
            truncate_chars(&issue.str_of("detail"), 200)
        ),
        &vec![issue.str_of("skill"), "resolution".to_string()],
    )?;
    Ok(())
}

// ---------- execution (R4/R5) ----------

#[derive(Debug)]
pub struct RunOutcome {
    pub ok: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub ms: i64,
    pub success_rule: String,
}

/// Shell-quote for safe `{input}` substitution.
fn shquote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Result of one policy-gated shell execution. Shared by skills (here),
/// goal steps (goals.rs), and cron actions.
#[derive(Debug)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub ms: i64,
    pub timed_out: bool,
}

/// Run a shell command through the command-pattern policy gate (journaled),
/// with a hard timeout. cwd = project root. Deny → Err containing
/// "blocked by policy".
pub fn exec_shell(c: &Ctx, cmd: &str, timeout_s: i64) -> R<ExecResult> {
    let dec = c.policy.check_command(cmd);
    c.log(
        "policy.decision",
        dec.to_json(&format!("command:{}", truncate_chars(cmd, 120))),
    )?;
    if let Decision::Deny { rule } = &dec {
        return Err(format!("command blocked by policy ({rule})"));
    }

    let timeout = Duration::from_secs(timeout_s.min(c.policy.max_timeout_s).max(1) as u64);
    let started = Instant::now();
    let mut child = ctx(
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&c.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn(),
        "spawn command",
    )?;

    // Drain pipes on threads so a chatty child can't deadlock on a full pipe
    // while we wait. Buffers are unbounded in theory, bounded in practice by
    // the command's own output; journaled excerpts are truncated.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break -9;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("wait on command: {e}")),
        }
    };
    Ok(ExecResult {
        exit_code,
        stdout: out_h.join().unwrap_or_default(),
        stderr: err_h.join().unwrap_or_default(),
        ms: started.elapsed().as_millis() as i64,
        timed_out,
    })
}

/// Execute a command skill through the policy gate, with timeout, metrics,
/// auto-issue on failure, and full journaling.
pub fn run(c: &Ctx, skill: &Skill, input: Option<&str>) -> R<RunOutcome> {
    if skill.kind() != "command" {
        return Err(format!(
            "skill '{}' is kind={} — run MCP skills via `soma mcp call`",
            skill.name(),
            skill.kind()
        ));
    }
    // Gate 1: autonomy.
    let exec = c.policy.check_execution("skill.run");
    c.log("policy.decision", exec.to_json(&format!("skill.run:{}", skill.name())))?;
    if !exec.allowed() {
        return Err(format!("blocked by policy ({})", exec.rule()));
    }
    // Build the command line.
    let mut cmd = skill.cmd();
    if let Some(inp) = input {
        if cmd.contains("{input}") {
            cmd = cmd.replace("{input}", &shquote(inp));
        } else {
            cmd = format!("{cmd} {}", shquote(inp));
        }
    }
    // Gate 2 + execution (shared executor journals the command decision).
    let res = match exec_shell(c, &cmd, skill.timeout_s()) {
        Ok(res) => res,
        Err(e) => {
            if e.contains("blocked by policy") {
                file_issue(c, &skill.name(), "policy_denied", &format!("{e}: {cmd}"))?;
            }
            return Err(e);
        }
    };
    let ExecResult {
        exit_code,
        stdout,
        stderr,
        ms,
        timed_out,
    } = res;

    // Success rule (R4).
    let (ok, success_rule) = if timed_out {
        (false, format!("timeout after {}s", skill.timeout_s().min(c.policy.max_timeout_s)))
    } else {
        match skill.manifest.get("success") {
            Some(s) if s.str_of("kind") == "contains" => {
                let needle = s.str_of("value");
                (
                    stdout.contains(&needle),
                    format!("stdout contains '{needle}'"),
                )
            }
            _ => (exit_code == 0, "exit code 0".to_string()),
        }
    };

    let detail = if ok {
        format!("exit {exit_code} in {ms}ms")
    } else if timed_out {
        format!(
            "timed out after {}s; stderr: {}",
            skill.timeout_s().min(c.policy.max_timeout_s),
            truncate_chars(&stderr, 200)
        )
    } else {
        format!("exit {exit_code}; stderr: {}", truncate_chars(&stderr, 200))
    };
    record_outcome(c, &skill.name(), ok, ms, &detail)?;

    Ok(RunOutcome {
        ok,
        exit_code,
        stdout,
        stderr,
        ms,
        success_rule,
    })
}

/// Builtin starter skills installed by `soma init --with-builtins` / docs.
pub fn builtin_manifests() -> Vec<Json> {
    let mk = |name: &str, purpose: &str, goal: &str, tags: Vec<&str>, cmd: &str, timeout: i64| {
        jobj(vec![
            ("name", jstr(name)),
            ("version", jint(1)),
            ("purpose", jstr(purpose)),
            ("goal", jstr(goal)),
            ("tags", jarr(tags.into_iter().map(jstr).collect())),
            ("kind", jstr("command")),
            (
                "run",
                jobj(vec![("cmd", jstr(cmd)), ("timeout_s", jint(timeout))]),
            ),
            ("success", jobj(vec![("kind", jstr("exit0")), ("value", jstr(""))])),
        ])
    };
    vec![
        mk(
            "cargo-build",
            "compile a rust project in release mode and surface compiler errors",
            "a successfully built release binary",
            vec!["rust", "build", "compile", "cargo"],
            "cargo build --release",
            300,
        ),
        mk(
            "cargo-test",
            "run the rust test suite and report failures",
            "all tests passing",
            vec!["rust", "test", "cargo", "verify"],
            "cargo test",
            300,
        ),
        mk(
            "git-status",
            "show working tree status of the current repository",
            "a summary of changed files",
            vec!["git", "status", "vcs"],
            "git status --short",
            30,
        ),
        mk(
            "disk-usage",
            "report disk usage of the project directory to spot bloat",
            "a per-directory size listing",
            vec!["disk", "usage", "size", "monitor"],
            "du -sh * | sort -rh | head -20",
            60,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    fn cmd_skill(name: &str, purpose: &str, tags: Vec<&str>, cmd: &str) -> Json {
        jobj(vec![
            ("name", jstr(name)),
            ("purpose", jstr(purpose)),
            ("goal", jstr("test outcome")),
            ("tags", jarr(tags.into_iter().map(jstr).collect())),
            ("kind", jstr("command")),
            ("run", jobj(vec![("cmd", jstr(cmd)), ("timeout_s", jint(5))])),
        ])
    }

    #[test]
    fn lint_catches_bad_manifests() {
        assert!(!lint(&cmd_skill("ok-skill", "does a useful thing", vec![], "echo hi")).is_empty() == false);
        let bad = jobj(vec![("name", jstr("bad name!"))]);
        let problems = lint(&bad);
        assert!(problems.iter().any(|p| p.starts_with("name")));
        assert!(problems.iter().any(|p| p.starts_with("purpose")));
    }

    #[test]
    fn add_run_metrics_roundtrip() {
        let (base, c) = temp_ctx();
        add(&c, cmd_skill("hello", "prints a greeting message", vec!["demo"], "echo hello-world"), false).unwrap();
        let s = find(&c, "hello").unwrap();
        let out = run(&c, &s, None).unwrap();
        assert!(out.ok);
        assert!(out.stdout.contains("hello-world"));
        let m = load_metrics(&c);
        assert_eq!(m.get("hello").unwrap().i_of("runs"), 1);
        assert_eq!(m.get("hello").unwrap().i_of("ok"), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn input_substitution_is_quoted() {
        let (base, c) = temp_ctx();
        add(&c, cmd_skill("echo-in", "echoes the provided input back", vec![], "echo {input}"), false).unwrap();
        let s = find(&c, "echo-in").unwrap();
        // injection attempt: would create a marker file if the input were
        // interpreted by the shell instead of arriving as one quoted literal
        let marker = std::env::temp_dir().join(format!("soma-inj-{}", new_id("x")));
        let inp = format!("a; touch {}; echo $(whoami) 'q'", marker.display());
        let out = run(&c, &s, Some(&inp)).unwrap();
        assert!(out.ok);
        assert!(out.stdout.contains("; touch"));     // literal, not executed
        assert!(out.stdout.contains("$(whoami)"));    // not expanded
        assert!(!marker.exists(), "injection executed!");
        // defense-in-depth: even quoted, destructive text trips the policy
        // string gate before any shell sees it
        let err = run(&c, &s, Some("hello; rm -rf / oops")).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn failure_files_issue_and_resolution_writes_lesson() {
        let (base, c) = temp_ctx();
        add(&c, cmd_skill("flaky", "always fails for testing purposes", vec![], "echo oops >&2; exit 3"), false).unwrap();
        let s = find(&c, "flaky").unwrap();
        let out = run(&c, &s, None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.exit_code, 3);
        let issues = list_issues(&c, true).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].str_of("detail").contains("oops"));

        resolve_issue(&c, &issues[0].str_of("id"), "fixed the flag").unwrap();
        assert!(list_issues(&c, true).unwrap().is_empty());
        let lessons = crate::knowledge::list(&c, 10).unwrap();
        assert!(lessons.iter().any(|l| l.str_of("kind") == "lesson"
            && l.str_of("title").contains("flaky")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn timeout_kills_and_records_failure() {
        let (base, c) = temp_ctx();
        let mut m = cmd_skill("sleepy", "sleeps far too long for its timeout", vec![], "sleep 30");
        m.set("run", jobj(vec![("cmd", jstr("sleep 30")), ("timeout_s", jint(1))]));
        add(&c, m, false).unwrap();
        let s = find(&c, "sleepy").unwrap();
        let started = std::time::Instant::now();
        let out = run(&c, &s, None).unwrap();
        assert!(!out.ok);
        assert!(started.elapsed().as_secs() < 10);
        assert!(out.success_rule.contains("timeout"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_denies_journal_and_issue() {
        let (base, c) = temp_ctx();
        add(&c, cmd_skill("danger", "tries to escalate privileges via sudo", vec![], "sudo whoami"), false).unwrap();
        let s = find(&c, "danger").unwrap();
        let err = run(&c, &s, None).unwrap_err();
        assert!(err.contains("blocked by policy"));
        let issues = list_issues(&c, true).unwrap();
        assert_eq!(issues[0].str_of("kind"), "policy_denied");
        // the deny decision itself is journaled
        let tail = c.journal().tail(10).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && !e.get("data").unwrap().b_of("allowed")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn observe_mode_blocks_execution() {
        let (base, mut c) = temp_ctx();
        add(&c, cmd_skill("hi", "prints hi for autonomy testing", vec![], "echo hi"), false).unwrap();
        c.policy.autonomy = "observe".into();
        let s = find(&c, "hi").unwrap();
        assert!(run(&c, &s, None).unwrap_err().contains("policy"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn shadowing_project_over_global() {
        let (base, c) = temp_ctx();
        add(&c, cmd_skill("dup", "global version of duplicate skill", vec![], "echo global"), true).unwrap();
        add(&c, cmd_skill("dup", "project version of duplicate skill", vec![], "echo project"), false).unwrap();
        let s = find(&c, "dup").unwrap();
        assert_eq!(s.scope, "project");
        assert_eq!(load_all(&c).iter().filter(|s| s.name() == "dup").count(), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn builtins_are_valid() {
        for m in builtin_manifests() {
            assert!(lint(&m).is_empty(), "builtin {} invalid", m.str_of("name"));
        }
    }
}
