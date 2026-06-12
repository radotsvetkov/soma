//! D9 — `soma wrap`: govern any agent CLI.
//!
//! Wrap policy-gates the *launch* of an arbitrary command (autonomy + command
//! deny globs, decided before anything spawns), tees the child's stdout/stderr
//! live to soma's own streams while hashing them incrementally, and journals
//! `wrap.start` / `wrap.end` receipts on the hash chain. stdin is inherited so
//! line-interactive children keep working.
//!
//! Honest limits (stated in --help too): wrap journals and gates the launch;
//! it does NOT syscall/network-sandbox the child. Pipes degrade full-screen
//! TUIs — target headless/print modes. For hard isolation run the child under
//! sandbox-exec/containers.
//!
//! Excerpt redaction is SHAPE-based (`name=value` / `name: value` / quoted
//! JSON `"name": "value"`), not a content scanner: it redacts values whose
//! KEY matches a `redact_keys` glob. Bare/unkeyed secrets in child output are
//! NOT scrubbed — operators must not assume arbitrary secrets a child prints
//! are removed. (No entropy scanning, by design — false positives would
//! corrupt the evidence excerpt.)
//!
//! The command gate is a DENYLIST of known-bad string forms matched over the
//! joined argv (F8) — NOT a semantic guard. Trivial spacing or flag variants
//! can evade a deny glob; treat it as a guardrail against obvious footguns,
//! not a sandbox.

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::policy::Decision;
use crate::project::Ctx;
use crate::sha256::{hex, Sha256};
use crate::util::*;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-stream excerpt bound: 2 KiB head + 2 KiB tail (D9).
const EXCERPT_BYTES: usize = 2048;

/// Env vars a `--env-strict` child still receives (plus LC_* and the
/// explicit `--env-pass` names) — the minimum for a CLI to behave sanely.
const ENV_BASELINE: [&str; 4] = ["PATH", "HOME", "TERM", "LANG"];

pub struct WrapOpts {
    pub label: Option<String>,
    /// 0 = no timeout (wrapped agent sessions may be long-lived);
    /// >0 is clamped to `policy.max_timeout_s`.
    pub timeout_s: i64,
    pub cwd: Option<PathBuf>,
    pub env_strict: bool,
    pub env_pass: Vec<String>,
    /// Child argv, verbatim (everything after `--`).
    pub cmd: Vec<String>,
}

#[derive(Debug)]
pub struct WrapOutcome {
    /// The `wrap.end` event data exactly as journaled (post-redaction).
    pub end: Json,
    /// Child exit as journaled: code, or -9 (timeout kill) / -1 (signal).
    pub exit: i64,
    pub timed_out: bool,
    /// What soma's own process should exit with: the child's code passed
    /// through; 124 on timeout (the coreutils `timeout` convention); 1 when
    /// the child died to a signal we didn't send.
    pub soma_exit: i32,
}

/// Spawn-gate, run, tee, journal. Refusals return Err *before* anything is
/// spawned; the policy decision that refused is already on the chain.
pub fn run(c: &Ctx, opts: WrapOpts) -> R<WrapOutcome> {
    // F2/F3 precedent (`mcp add`): refuse an empty or flag-looking command
    // before any gate is even consulted.
    let Some(cmd0) = opts.cmd.first() else {
        return Err("wrap: missing child command — usage: soma wrap [flags] -- <cmd> [args...]".into());
    };
    if cmd0.trim().is_empty() {
        return Err("wrap: child command must not be empty".into());
    }
    if cmd0.starts_with('-') {
        return Err(format!(
            "wrap: child command '{cmd0}' looks like a flag — soma's own flags go before `--`"
        ));
    }
    let base = cmd0.rsplit(['/', '\\']).next().unwrap_or(cmd0);
    let label = opts.label.clone().unwrap_or_else(|| base.to_string());
    let cmdline = opts.cmd.join(" ");

    // Gate 1: autonomy — observe never spawns (decision journaled either way).
    let exec = c.policy.check_execution("wrap.run");
    c.log("policy.decision", exec.to_json(&format!("wrap.run:{label}")))?;
    if !exec.allowed() {
        return Err(format!("blocked by policy ({})", exec.rule()));
    }
    // Gate 2: command patterns over the FULL child command line. The gate
    // sees the raw line; the journaled copy passes value-level redaction so
    // a secret handed to the agent as an argument never touches disk (R3).
    let dec = c.policy.check_command(&cmdline);
    let logged_cmdline = redact_text(&cmdline, &c.policy.redact_keys);
    c.log(
        "policy.decision",
        dec.to_json(&format!("command:{}", truncate_chars(&logged_cmdline, 120))),
    )?;
    if let Decision::Deny { rule } = &dec {
        return Err(format!("command blocked by policy ({rule})"));
    }

    let cwd = match &opts.cwd {
        Some(d) => d.clone(),
        None => ctx(std::env::current_dir(), "cwd")?,
    };
    let mut command = Command::new(cmd0);
    command
        .args(&opts.cmd[1..])
        .current_dir(&cwd)
        .stdin(Stdio::inherit()) // line-interactive children keep working
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Env scoping: only NAMES are ever journaled; values never leave the
    // process environment.
    let env_sensitive = if opts.env_strict {
        command.env_clear();
        let mut passed: Vec<String> = Vec::new();
        for (name, value) in std::env::vars() {
            let keep = ENV_BASELINE.contains(&name.as_str())
                || name.starts_with("LC_")
                || opts.env_pass.iter().any(|p| p == &name);
            if keep {
                command.env(&name, value);
                passed.push(name);
            }
        }
        sensitive_names(passed.into_iter(), &c.policy.redact_keys)
    } else {
        sensitive_names(std::env::vars().map(|(n, _)| n), &c.policy.redact_keys)
    };

    let started = Instant::now();
    let mut child = ctx(command.spawn(), "spawn wrapped command")?;
    // Capture the pid up front: it's carried into BOTH wrap.start and wrap.end
    // so the aiact period-of-use pass can pair starts↔ends on (label, pid),
    // not label alone (F9). child.id() is unreliable after the child is reaped.
    let pid = child.id() as i64;
    // Crash-safe receipt: even if soma is killed mid-run, the start of the
    // wrapped session is already on the chain.
    c.log(
        "wrap.start",
        jobj(vec![
            ("label", jstr(&label)),
            ("cmd", jstr(cmd0)),
            // args journaled through value-level redaction: `--api-key=...`
            // style secrets must not land on the chain (R3).
            (
                "args",
                jarr(
                    opts.cmd[1..]
                        .iter()
                        .map(|a| jstr(&redact_text(a, &c.policy.redact_keys)))
                        .collect(),
                ),
            ),
            ("cwd", jstr(cwd.to_string_lossy().as_ref())),
            (
                "env_sensitive",
                jarr(env_sensitive.iter().map(|n| jstr(n)).collect()),
            ),
            ("pid", jint(pid)),
        ]),
    )?;

    // Tee threads: live bytes out first, bookkeeping second — a chatty child
    // can't deadlock on a full pipe and the agent stays responsive.
    let out_h = spawn_tee(
        child.stdout.take().ok_or("child stdout not piped")?,
        std::io::stdout(),
    );
    let err_h = spawn_tee(
        child.stderr.take().ok_or("child stderr not piped")?,
        std::io::stderr(),
    );

    let timeout = if opts.timeout_s > 0 {
        Some(Duration::from_secs(
            opts.timeout_s.min(c.policy.max_timeout_s).max(1) as u64,
        ))
    } else {
        None
    };
    let mut timed_out = false;
    let exit: i64 = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1) as i64,
            Ok(None) => {
                if let Some(t) = timeout {
                    if started.elapsed() > t {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break -9;
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("wait on wrapped command: {e}")),
        }
    };
    let out_stats = out_h.join().map_err(|_| "stdout tee thread panicked")?;
    let err_stats = err_h.join().map_err(|_| "stderr tee thread panicked")?;
    let duration_ms = started.elapsed().as_millis() as i64;

    // Excerpts pass text-level redaction here (raw child output is opaque to
    // the key-based events::redact), then Journal::append's key redaction on
    // top. Hashes stay over the RAW bytes — that's what a verifier recomputes.
    let event = c.log(
        "wrap.end",
        jobj(vec![
            ("label", jstr(&label)),
            // pid carried from wrap.start so aiact pairs on (label, pid) — F9.
            ("pid", jint(pid)),
            ("exit", jint(exit)),
            ("duration_ms", jint(duration_ms)),
            ("stdout_sha256", jstr(&out_stats.sha256)),
            ("stderr_sha256", jstr(&err_stats.sha256)),
            ("stdout_bytes", jint(out_stats.bytes as i64)),
            ("stderr_bytes", jint(err_stats.bytes as i64)),
            (
                "stdout_excerpt",
                jstr(&redact_text(&out_stats.excerpt(), &c.policy.redact_keys)),
            ),
            (
                "stderr_excerpt",
                jstr(&redact_text(&err_stats.excerpt(), &c.policy.redact_keys)),
            ),
            ("timed_out", jbool(timed_out)),
        ]),
    )?;
    let soma_exit = if timed_out {
        124
    } else if (0..=255).contains(&exit) {
        exit as i32
    } else {
        1
    };
    Ok(WrapOutcome {
        end: event.get("data").cloned().unwrap_or(Json::Null),
        exit,
        timed_out,
        soma_exit,
    })
}

/// Env var names (sorted) whose lowercased name matches a redaction glob —
/// the `env_sensitive` field of `wrap.start`.
fn sensitive_names(names: impl Iterator<Item = String>, patterns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .filter(|n| {
            let lower = n.to_lowercase();
            patterns.iter().any(|p| glob_match(p, &lower))
        })
        .collect();
    out.sort();
    out
}

// ---------- tee capture ----------

struct TeeStats {
    sha256: String,
    bytes: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl TeeStats {
    /// Reassemble the bounded excerpt: the whole output if it fit in the
    /// head+tail windows, otherwise head + an honest omission marker + tail.
    fn excerpt(&self) -> String {
        excerpt_of(&self.head, &self.tail, self.bytes as usize)
    }
}

fn excerpt_of(head: &[u8], tail: &[u8], total: usize) -> String {
    let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if total <= EXCERPT_BYTES {
        lossy(head)
    } else if total <= EXCERPT_BYTES * 2 {
        // head holds the first 2 KiB; tail holds the last 2 KiB — splice the
        // non-overlapping back portion on.
        let missing = total - head.len();
        format!("{}{}", lossy(head), lossy(&tail[tail.len() - missing..]))
    } else {
        format!(
            "{}\n…[{} bytes omitted]…\n{}",
            lossy(head),
            total - head.len() - tail.len(),
            lossy(tail)
        )
    }
}

/// Reader thread: stream child bytes to `dst` as they arrive, while hashing
/// (incremental SHA-256), counting, and keeping head/tail excerpt windows.
fn spawn_tee(
    mut src: impl Read + Send + 'static,
    mut dst: impl Write + Send + 'static,
) -> std::thread::JoinHandle<TeeStats> {
    std::thread::spawn(move || {
        let mut sha = Sha256::new();
        let mut bytes: u64 = 0;
        let mut head: Vec<u8> = Vec::new();
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = &buf[..n];
            // Live tee first — the wrapped agent stays interactive even if
            // bookkeeping were ever slow.
            let _ = dst.write_all(chunk);
            let _ = dst.flush();
            sha.update(chunk);
            bytes += n as u64;
            if head.len() < EXCERPT_BYTES {
                let take = (EXCERPT_BYTES - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
            }
            if chunk.len() >= EXCERPT_BYTES {
                tail.clear();
                tail.extend_from_slice(&chunk[chunk.len() - EXCERPT_BYTES..]);
            } else {
                let overflow = (tail.len() + chunk.len()).saturating_sub(EXCERPT_BYTES);
                tail.drain(..overflow);
                tail.extend_from_slice(chunk);
            }
        }
        TeeStats {
            sha256: hex(&sha.finish()),
            bytes,
            head,
            tail,
        }
    })
}

// ---------- excerpt redaction ----------

/// Redact `NAME=value`, `name: value` and `"name": "value"` shapes whose name
/// matches a redaction glob. Excerpts are raw child output, so the key-based
/// `events::redact` can't see inside them — this is the value-level
/// counterpart, applied before the excerpt is journaled.
pub fn redact_text(text: &str, patterns: &[String]) -> String {
    text.split('\n')
        .map(|line| redact_line(line, patterns))
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str, patterns: &[String]) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '=' || c == ':' {
            // Identifier immediately before the separator (closing quote of a
            // JSON key skipped, e.g. `"api_key":`).
            let mut k = i;
            while k > 0 && (chars[k - 1] == '"' || chars[k - 1] == '\'') {
                k -= 1;
            }
            let name_end = k;
            while k > 0
                && (chars[k - 1].is_ascii_alphanumeric() || matches!(chars[k - 1], '_' | '-' | '.'))
            {
                k -= 1;
            }
            let name: String = chars[k..name_end].iter().collect::<String>().to_lowercase();
            if !name.is_empty() && patterns.iter().any(|p| glob_match(p, &name)) {
                out.push(c);
                i += 1;
                while i < chars.len() && chars[i] == ' ' {
                    out.push(' ');
                    i += 1;
                }
                if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                    // quoted value: redact up to the matching quote
                    let q = chars[i];
                    i += 1;
                    while i < chars.len() && chars[i] != q {
                        i += 1;
                    }
                    if i < chars.len() {
                        i += 1; // closing quote
                    }
                    out.push(q);
                    out.push_str("[redacted]");
                    out.push(q);
                } else {
                    // bare value: redact up to whitespace/comma/quote
                    let start = i;
                    while i < chars.len()
                        && !chars[i].is_whitespace()
                        && chars[i] != ','
                        && chars[i] != '"'
                    {
                        i += 1;
                    }
                    if i > start {
                        out.push_str("[redacted]");
                    }
                }
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;
    use crate::sha256::sha256_hex;

    fn opts(cmd: &[&str]) -> WrapOpts {
        WrapOpts {
            label: None,
            timeout_s: 0,
            cwd: None,
            env_strict: false,
            env_pass: vec![],
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn kinds(c: &Ctx, n: usize) -> Vec<String> {
        c.journal()
            .tail(n)
            .unwrap()
            .iter()
            .map(|e| e.str_of("kind"))
            .collect()
    }

    #[test]
    fn observe_refuses_journaled_nothing_spawned() {
        let (base, mut c) = temp_ctx();
        c.policy.autonomy = "observe".into();
        let marker = base.join("spawned-anyway");
        let touch = format!("touch {}", marker.display());
        let err = run(&c, opts(&["/bin/sh", "-c", &touch])).unwrap_err();
        assert!(err.contains("blocked by policy"), "{err}");
        assert!(!marker.exists(), "child was spawned despite observe!");
        let tail = c.journal().tail(5).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && !e.get("data").unwrap().b_of("allowed")));
        assert!(!kinds(&c, 10).iter().any(|k| k == "wrap.start"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn deny_listed_command_refused_at_spawn() {
        let (base, c) = temp_ctx();
        let err = run(&c, opts(&["sudo", "whoami"])).unwrap_err();
        assert!(err.contains("command blocked by policy"), "{err}");
        assert!(err.contains("deny_commands"), "{err}");
        assert!(!kinds(&c, 10).iter().any(|k| k == "wrap.start"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn empty_and_flag_looking_cmd_refused() {
        let (base, c) = temp_ctx();
        assert!(run(&c, opts(&[])).unwrap_err().contains("missing child command"));
        assert!(run(&c, opts(&["-p", "hi"]))
            .unwrap_err()
            .contains("looks like a flag"));
        // refused before any gate: nothing journaled for these
        assert!(!kinds(&c, 10).iter().any(|k| k == "policy.decision"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn sha256_bytes_and_receipts_for_echo() {
        let (base, c) = temp_ctx();
        let out = run(&c, opts(&["/bin/echo", "hello"])).unwrap();
        assert_eq!(out.exit, 0);
        assert_eq!(out.soma_exit, 0);
        assert!(!out.timed_out);
        assert_eq!(out.end.i_of("stdout_bytes"), 6); // "hello\n"
        assert_eq!(out.end.str_of("stdout_sha256"), sha256_hex(b"hello\n"));
        assert_eq!(out.end.i_of("stderr_bytes"), 0);
        assert_eq!(out.end.str_of("stderr_sha256"), sha256_hex(b""));
        assert!(out.end.str_of("stdout_excerpt").contains("hello"));
        assert_eq!(out.end.str_of("label"), "echo"); // defaults to basename
        let ks = kinds(&c, 10);
        assert!(ks.iter().any(|k| k == "wrap.start"));
        assert!(ks.iter().any(|k| k == "wrap.end"));
        assert!(c.journal().verify().unwrap().ok);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn exit_code_propagates() {
        let (base, c) = temp_ctx();
        let out = run(&c, opts(&["/bin/sh", "-c", "exit 7"])).unwrap();
        assert_eq!(out.exit, 7);
        assert_eq!(out.soma_exit, 7);
        assert_eq!(out.end.i_of("exit"), 7);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn timeout_kills_and_flags() {
        let (base, c) = temp_ctx();
        let mut o = opts(&["/bin/sleep", "30"]);
        o.timeout_s = 1;
        let started = std::time::Instant::now();
        let out = run(&c, o).unwrap();
        assert!(started.elapsed().as_secs() < 10);
        assert!(out.timed_out);
        assert!(out.end.b_of("timed_out"));
        assert_eq!(out.exit, -9);
        assert_eq!(out.soma_exit, 124);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn excerpt_redacts_secret_values() {
        let (base, c) = temp_ctx();
        let out = run(
            &c,
            opts(&["/bin/sh", "-c", "echo api_key=sk-ant-VERYSECRET123"]),
        )
        .unwrap();
        let ex = out.end.str_of("stdout_excerpt");
        assert!(ex.contains("[redacted]"), "{ex}");
        assert!(!ex.contains("VERYSECRET123"), "{ex}");
        // the secret never reached disk in any field
        let raw = std::fs::read_to_string(&c.journal().path).unwrap();
        assert!(!raw.contains("VERYSECRET123"));
        // ...but the hash still covers the RAW bytes
        assert_eq!(
            out.end.str_of("stdout_sha256"),
            sha256_hex(b"api_key=sk-ant-VERYSECRET123\n")
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn env_strict_drops_and_env_pass_restores() {
        let (base, c) = temp_ctx();
        std::env::set_var("SOMA_WRAP_TEST_MARKER", "present-123");
        let shellcmd = r#"echo ${SOMA_WRAP_TEST_MARKER:-missing}"#;
        // default: inherited
        let out = run(&c, opts(&["/bin/sh", "-c", shellcmd])).unwrap();
        assert!(out.end.str_of("stdout_excerpt").contains("present-123"));
        // strict: dropped
        let mut o = opts(&["/bin/sh", "-c", shellcmd]);
        o.env_strict = true;
        let out = run(&c, o).unwrap();
        assert!(out.end.str_of("stdout_excerpt").contains("missing"));
        // strict + --env-pass: restored
        let mut o = opts(&["/bin/sh", "-c", shellcmd]);
        o.env_strict = true;
        o.env_pass = vec!["SOMA_WRAP_TEST_MARKER".into()];
        let out = run(&c, o).unwrap();
        assert!(out.end.str_of("stdout_excerpt").contains("present-123"));
        std::env::remove_var("SOMA_WRAP_TEST_MARKER");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn env_sensitive_journals_names_never_values() {
        let (base, c) = temp_ctx();
        std::env::set_var("SOMA_TEST_FAKE_TOKEN", "tok-hush-value");
        let out = run(&c, opts(&["/bin/echo", "hi"])).unwrap();
        assert_eq!(out.exit, 0);
        let raw = std::fs::read_to_string(&c.journal().path).unwrap();
        assert!(raw.contains("SOMA_TEST_FAKE_TOKEN")); // name listed
        assert!(!raw.contains("tok-hush-value")); // value never
        let start = c
            .journal()
            .tail(5)
            .unwrap()
            .into_iter()
            .find(|e| e.str_of("kind") == "wrap.start")
            .unwrap();
        assert!(start
            .get("data")
            .unwrap()
            .strs_of("env_sensitive")
            .iter()
            .any(|n| n == "SOMA_TEST_FAKE_TOKEN"));
        std::env::remove_var("SOMA_TEST_FAKE_TOKEN");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn excerpt_windows() {
        // fits entirely
        assert_eq!(excerpt_of(b"abc", b"abc", 3), "abc");
        // head+tail overlap region splices without duplication
        let total = 3000usize; // EXCERPT < total <= 2*EXCERPT
        let data: Vec<u8> = (0..total).map(|i| b'a' + (i % 26) as u8).collect();
        let head = &data[..EXCERPT_BYTES];
        let tail = &data[total - EXCERPT_BYTES..];
        let ex = excerpt_of(head, tail, total);
        assert_eq!(ex.len(), total);
        assert_eq!(ex.as_bytes(), &data[..]);
        // big output: omission marker carries the gap size
        let ex = excerpt_of(&[b'h'; EXCERPT_BYTES], &[b't'; EXCERPT_BYTES], 10000);
        assert!(ex.contains("…[5904 bytes omitted]…"), "{ex}");
        assert!(ex.starts_with('h'));
        assert!(ex.ends_with('t'));
    }

    #[test]
    fn redact_text_shapes() {
        let pats: Vec<String> = vec!["*key*".into(), "*token*".into(), "*password*".into()];
        assert_eq!(
            redact_text("api_key=sk-123 rest", &pats),
            "api_key=[redacted] rest"
        );
        assert_eq!(
            redact_text(r#""token": "abc def""#, &pats),
            r#""token": "[redacted]""#
        );
        assert_eq!(
            redact_text("PASSWORD: hunter2 more", &pats),
            "PASSWORD: [redacted] more"
        );
        // non-matching names and bare URLs pass through
        assert_eq!(redact_text("path=/usr/bin", &pats), "path=/usr/bin");
        assert_eq!(
            redact_text("see https://example.com/x", &pats),
            "see https://example.com/x"
        );
        // multi-line: each line scanned independently
        let multi = "ok line\nmy_token=secret\nfin";
        assert_eq!(redact_text(multi, &pats), "ok line\nmy_token=[redacted]\nfin");
    }

    #[test]
    fn default_redact_keys_cover_auth_and_bearer() {
        // F6: the default policy must scrub Authorization/auth and bearer-keyed
        // values reaching disk via the wrap excerpt.
        let p = crate::policy::Policy::default_policy();
        assert!(p.redact_keys.iter().any(|k| k == "*auth*"), "default redact_keys must include *auth*");
        assert!(p.redact_keys.iter().any(|k| k == "*bearer*"), "default redact_keys must include *bearer*");
        // A `bearer`-keyed value is redacted by the default keys.
        assert_eq!(
            redact_text("bearer=xyz123 rest", &p.redact_keys),
            "bearer=[redacted] rest"
        );
        // An `Authorization:` header value too (key matches *auth*).
        assert_eq!(
            redact_text("Authorization: secrettoken more", &p.redact_keys),
            "Authorization: [redacted] more"
        );
    }
}
