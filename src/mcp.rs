//! R16 — MCP client: newline-delimited JSON-RPC 2.0 over stdio.
//!
//! Servers are declared in `.soma/mcp.json`:
//!   {"servers": {"name": {"command": "...", "args": [...], "env": {...}}}}
//!
//! `soma mcp import <server>` materializes the server's tools as `kind: mcp`
//! skills, so the neuro selector can rank and explain them next to native
//! skills. Every RPC round-trip is journaled.

use crate::json::{jarr, jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::util::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Json>,
    pub server: String,
    next_id: i64,
}

fn servers_config(c: &Ctx) -> R<Json> {
    let s = read_to_string(&c.mcp_path()).map_err(|_| {
        format!(
            "no {} — declare servers there first",
            c.mcp_path().display()
        )
    })?;
    crate::json::parse(&s).map_err(|e| format!("mcp.json: {e}"))
}

pub fn list_servers(c: &Ctx) -> Vec<(String, Json)> {
    servers_config(c)
        .ok()
        .and_then(|j| j.get("servers").and_then(|s| s.obj().cloned()))
        .unwrap_or_default()
}

impl McpClient {
    /// Spawn the server process and complete the MCP initialize handshake.
    pub fn connect(c: &Ctx, server: &str) -> R<McpClient> {
        let cfg = servers_config(c)?;
        let entry = cfg
            .get("servers")
            .and_then(|s| s.get(server))
            .cloned()
            .ok_or_else(|| format!("no MCP server '{server}' in mcp.json"))?;
        let command = entry.str_of("command");
        if command.is_empty() {
            return Err(format!("server '{server}' has no command"));
        }
        let args = entry.strs_of("args");

        // Defense in depth (gate 1/2): the *binary* is gated by a dedicated,
        // stricter allowlist than general `allow_commands`. Adding a server is
        // reachable from the cockpit webview, so a one-liner like
        // `--cmd /bin/sh --arg -c --arg '<payload>'` must not spawn just
        // because `allow_commands` ships permissive ("*"). Journaled either way.
        let spawn_dec = c.policy.check_mcp_command(&command);
        c.log(
            "policy.decision",
            spawn_dec.to_json(&format!("mcp.spawn:{command}")),
        )?;
        if !spawn_dec.allowed() {
            return Err(format!(
                "mcp server '{server}': command '{command}' is not on mcp_allow_commands \
                 ({}) — if you trust it, widen the policy: \
                 soma policy set mcp_allow_commands '[...]'",
                spawn_dec.rule()
            ));
        }

        // Gate 2/2: spawning an MCP server is also command execution, so the
        // general deny list still applies to the full command line.
        let full_cmd = format!("{command} {}", args.join(" "));
        let dec = c.policy.check_command(&full_cmd);
        c.log(
            "policy.decision",
            dec.to_json(&format!("mcp.connect:{}", truncate_chars(&full_cmd, 120))),
        )?;
        if !dec.allowed() {
            return Err(format!("mcp server blocked by policy ({})", dec.rule()));
        }

        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .current_dir(&c.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(env) = entry.get("env").and_then(|e| e.obj()) {
            for (k, v) in env {
                cmd.env(k, v.s().unwrap_or(""));
            }
        }
        let mut child = ctx(cmd.spawn(), &format!("spawn mcp server '{server}'"))?;
        let stdin = child.stdin.take().ok_or("mcp stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("mcp stdout unavailable")?;

        // Reader thread: every parseable JSON line goes down the channel.
        let (tx, rx) = mpsc::channel::<Json>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(j) = crate::json::parse(&line) {
                    if tx.send(j).is_err() {
                        break;
                    }
                }
            }
        });

        let mut client = McpClient {
            child,
            stdin,
            rx,
            server: server.to_string(),
            next_id: 1,
        };
        let init = client.request(
            c,
            "initialize",
            jobj(vec![
                ("protocolVersion", jstr(PROTOCOL_VERSION)),
                ("capabilities", jobj(vec![])),
                (
                    "clientInfo",
                    jobj(vec![
                        ("name", jstr("soma")),
                        ("version", jstr(crate::project::SOMA_VERSION)),
                    ]),
                ),
            ]),
        )?;
        let _server_info = init.get("serverInfo").cloned();
        client.notify("notifications/initialized", jobj(vec![]))?;
        Ok(client)
    }

    fn send_line(&mut self, msg: &Json) -> R<()> {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write to mcp server: {e}"))
    }

    fn notify(&mut self, method: &str, params: Json) -> R<()> {
        self.send_line(&jobj(vec![
            ("jsonrpc", jstr("2.0")),
            ("method", jstr(method)),
            ("params", params),
        ]))
    }

    /// Send a request and wait for the matching response id, skipping any
    /// notifications the server emits in between. Journaled.
    pub fn request(&mut self, c: &Ctx, method: &str, params: Json) -> R<Json> {
        let id = self.next_id;
        self.next_id += 1;
        let started = std::time::Instant::now();
        self.send_line(&jobj(vec![
            ("jsonrpc", jstr("2.0")),
            ("id", jint(id)),
            ("method", jstr(method)),
            ("params", params),
        ]))?;
        let deadline = std::time::Instant::now() + RPC_TIMEOUT;
        let result = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Err(format!("mcp '{}' timed out on {method}", self.server));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => {
                    if msg.get("id").and_then(|i| i.i()) == Some(id) {
                        if let Some(err) = msg.get("error") {
                            break Err(format!(
                                "mcp error {}: {}",
                                err.i_of("code"),
                                err.str_of("message")
                            ));
                        }
                        break Ok(msg.get("result").cloned().unwrap_or(Json::Null));
                    }
                    // else: notification or unrelated message — keep reading
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    break Err(format!("mcp '{}' timed out on {method}", self.server))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(format!("mcp server '{}' closed its pipe", self.server))
                }
            }
        };
        c.log(
            "mcp.rpc",
            jobj(vec![
                ("server", jstr(&self.server)),
                ("method", jstr(method)),
                ("ms", jint(started.elapsed().as_millis() as i64)),
                ("ok", crate::json::jbool(result.is_ok())),
            ]),
        )?;
        result
    }

    pub fn tools_list(&mut self, c: &Ctx) -> R<Vec<Json>> {
        let res = self.request(c, "tools/list", jobj(vec![]))?;
        Ok(res.arr_of("tools"))
    }

    pub fn tools_call(&mut self, c: &Ctx, tool: &str, args: Json) -> R<Json> {
        self.request(
            c,
            "tools/call",
            jobj(vec![("name", jstr(tool)), ("arguments", args)]),
        )
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Flatten an MCP tool result's content blocks to text.
pub fn result_text(result: &Json) -> String {
    result
        .arr_of("content")
        .iter()
        .filter(|b| b.str_of("type") == "text")
        .map(|b| b.str_of("text"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Call an MCP tool and record the outcome against its skill name (if it was
/// imported), so MCP tools build reliability history like any other skill.
pub fn call(c: &Ctx, server: &str, tool: &str, args: Json) -> R<String> {
    let exec = c.policy.check_execution("mcp.call");
    c.log(
        "policy.decision",
        exec.to_json(&format!("mcp.call:{server}/{tool}")),
    )?;
    if !exec.allowed() {
        return Err(format!("blocked by policy ({})", exec.rule()));
    }
    let started = std::time::Instant::now();
    let mut client = McpClient::connect(c, server)?;
    let result = client.tools_call(c, tool, args);
    let ms = started.elapsed().as_millis() as i64;
    let skill_name = skill_name_for(server, tool);
    if crate::skills::find(c, &skill_name).is_ok() {
        let (ok, detail) = match &result {
            Ok(r) => (!r.b_of("isError"), truncate_chars(&result_text(r), 120)),
            Err(e) => (false, e.clone()),
        };
        crate::skills::record_outcome(c, &skill_name, ok, ms, &detail)?;
    }
    let r = result?;
    if r.b_of("isError") {
        return Err(format!("tool error: {}", result_text(&r)));
    }
    Ok(result_text(&r))
}

pub fn skill_name_for(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    };
    format!("mcp-{}-{}", sanitize(server), sanitize(tool))
}

/// Add a server entry to `.soma/mcp.json`, creating the file if absent.
/// Errors if the name already exists.
pub fn add_server(c: &Ctx, name: &str, command: &str, args: &[String]) -> R<()> {
    let mcp_path = c.mcp_path();
    let mut root = if mcp_path.exists() {
        let s = read_to_string(&mcp_path).map_err(|e| format!("mcp.json: {e}"))?;
        crate::json::parse(&s).map_err(|e| format!("mcp.json: {e}"))?
    } else {
        jobj(vec![])
    };

    // Ensure "servers" key is an object.
    if root.get("servers").is_none() {
        root.set("servers", jobj(vec![]));
    }

    // Check for duplicate.
    if root.get("servers").and_then(|s| s.get(name)).is_some() {
        return Err(format!(
            "mcp: server '{name}' already exists — remove it first"
        ));
    }

    // F1 (CRITICAL): gate the command at the MUTATION boundary, not only at
    // connect. The same stricter mcp allowlist that guards spawning must guard
    // persistence — otherwise a policy-denied command (e.g. `sudo`) lands in
    // mcp.json and the audit trail shows a clean mcp.add. Journal the decision
    // either way (consistent with config/policy refusal journaling).
    let dec = c.policy.check_mcp_command(command);
    c.log("policy.decision", dec.to_json(&format!("mcp.add:{command}")))?;
    if !dec.allowed() {
        return Err(format!(
            "mcp add: command '{command}' blocked by policy ({}) — not added",
            dec.rule()
        ));
    }

    // Build the server entry.
    let entry = jobj(vec![
        ("command", jstr(command)),
        (
            "args",
            Json::Arr(args.iter().map(|a| jstr(a.as_str())).collect()),
        ),
    ]);

    // Insert into servers object.
    if let Json::Obj(ref mut pairs) = root {
        if let Some((_, servers_val)) = pairs.iter_mut().find(|(k, _)| k == "servers") {
            if let Json::Obj(ref mut srv_pairs) = servers_val {
                srv_pairs.push((name.to_string(), entry));
            }
        }
    }

    atomic_write(&mcp_path, root.pretty().as_bytes())
        .map_err(|e| format!("mcp.json write: {e}"))?;

    c.log(
        "mcp.add",
        jobj(vec![
            ("server", jstr(name)),
            ("command", jstr(command)),
            (
                "args",
                Json::Arr(args.iter().map(|a| jstr(a.as_str())).collect()),
            ),
        ]),
    )?;
    Ok(())
}

/// Remove a server entry from `.soma/mcp.json`.
/// Errors if the server is not present.
pub fn remove_server(c: &Ctx, name: &str) -> R<()> {
    let mcp_path = c.mcp_path();
    let s = read_to_string(&mcp_path)
        .map_err(|_| format!("mcp: server '{name}' not found — no mcp.json"))?;
    let mut root = crate::json::parse(&s).map_err(|e| format!("mcp.json: {e}"))?;

    // Verify server exists.
    if root.get("servers").and_then(|s| s.get(name)).is_none() {
        return Err(format!("mcp: server '{name}' not found"));
    }

    // Remove from servers object.
    if let Json::Obj(ref mut pairs) = root {
        if let Some((_, servers_val)) = pairs.iter_mut().find(|(k, _)| k == "servers") {
            if let Json::Obj(ref mut srv_pairs) = servers_val {
                srv_pairs.retain(|(k, _)| k != name);
            }
        }
    }

    atomic_write(&mcp_path, root.pretty().as_bytes())
        .map_err(|e| format!("mcp.json write: {e}"))?;

    c.log("mcp.remove", jobj(vec![("server", jstr(name))]))?;
    Ok(())
}

/// Import a server's tools as `kind: mcp` skills (R16 ↔ R4/R6).
pub fn import(c: &Ctx, server: &str) -> R<Vec<String>> {
    let mut client = McpClient::connect(c, server)?;
    let tools = client.tools_list(c)?;
    let mut imported = Vec::new();
    for t in &tools {
        let tool = t.str_of("name");
        let desc = t.str_of("description");
        let manifest = jobj(vec![
            ("name", jstr(&skill_name_for(server, &tool))),
            (
                "purpose",
                jstr(if desc.len() >= 8 {
                    desc.clone()
                } else {
                    format!("invoke MCP tool {tool} on server {server}")
                }),
            ),
            (
                "goal",
                jstr(&format!(
                    "result of MCP tool '{tool}' from server '{server}'"
                )),
            ),
            ("tags", jarr(vec![jstr("mcp"), jstr(server), jstr(&tool)])),
            ("kind", jstr("mcp")),
            (
                "run",
                jobj(vec![("server", jstr(server)), ("tool", jstr(&tool))]),
            ),
        ]);
        crate::skills::add(c, manifest, false)?;
        imported.push(tool);
    }
    c.log(
        "mcp.import",
        jobj(vec![
            ("server", jstr(server)),
            ("tools", jint(imported.len() as i64)),
        ]),
    )?;
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    fn python3_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn setup_mock_server(c: &Ctx) {
        let script = format!("{}/tests/mock_mcp.py", env!("CARGO_MANIFEST_DIR"));
        atomic_write(
            &c.mcp_path(),
            jobj(vec![(
                "servers",
                jobj(vec![(
                    "mock",
                    jobj(vec![
                        ("command", jstr("python3")),
                        ("args", jarr(vec![jstr(&script)])),
                    ]),
                )]),
            )])
            .pretty()
            .as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn handshake_list_call_against_mock_server() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let (base, mut c) = temp_ctx();
        // The mock server runs via python3, which is (correctly) not a default
        // mcp launcher — widen the test policy to permit it.
        c.policy.mcp_allow_commands.push("python3".into());
        setup_mock_server(&c);
        let mut client = McpClient::connect(&c, "mock").unwrap();
        let tools = client.tools_list(&c).unwrap();
        assert!(tools.iter().any(|t| t.str_of("name") == "add"));
        let res = client
            .tools_call(&c, "add", jobj(vec![("a", jint(2)), ("b", jint(3))]))
            .unwrap();
        assert_eq!(result_text(&res), "5");
        // rpc round-trips journaled
        let tail = c.journal().tail(10).unwrap();
        assert!(tail.iter().any(|e| e.str_of("kind") == "mcp.rpc"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn import_makes_selectable_skills() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let (base, mut c) = temp_ctx();
        c.policy.mcp_allow_commands.push("python3".into());
        setup_mock_server(&c);
        let imported = import(&c, "mock").unwrap();
        assert!(imported.contains(&"add".to_string()));
        let s = crate::skills::find(&c, "mcp-mock-add").unwrap();
        assert_eq!(s.kind(), "mcp");
        // the selector can rank it
        let sel = crate::neuro::select(&c, "add two numbers together").unwrap();
        assert_eq!(sel.candidates[0].name, "mcp-mock-add");
        // and calling through the mcp path records metrics on the skill
        let out = call(
            &c,
            "mock",
            "add",
            jobj(vec![("a", jint(20)), ("b", jint(22))]),
        )
        .unwrap();
        assert_eq!(out, "42");
        let m = crate::skills::load_metrics(&c);
        assert_eq!(m.get("mcp-mock-add").unwrap().i_of("runs"), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn missing_config_and_unknown_server_error_cleanly() {
        let (base, c) = temp_ctx();
        assert!(McpClient::connect(&c, "ghost").is_err());
        setup_mock_server(&c);
        assert!(McpClient::connect(&c, "ghost")
            .unwrap_err()
            .contains("no MCP server"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn shell_injection_server_is_denied_before_spawn() {
        // The reported RCE: register /bin/sh as a server, then `mcp tools`/
        // `mcp import` (both → connect) spawn it. The mcp_allow_commands gate
        // must refuse to spawn, and the payload must never run.
        let (base, c) = temp_ctx();
        let marker = base.join("soma_pwned_marker");
        // F1: the denied command is refused at the ADD (mutation) boundary —
        // it never reaches mcp.json, so it can never be spawned. Stronger than
        // the old deny-at-connect: the payload is never even persisted.
        let err = add_server(
            &c,
            "evil",
            "/bin/sh",
            &["-c".to_string(), format!("touch {}", marker.display())],
        )
        .unwrap_err();
        assert!(
            err.contains("blocked by policy"),
            "expected add-time policy denial, got: {err}"
        );
        assert!(!marker.exists(), "shell payload must not run");
        // Not persisted: connect can't use a server that was never added.
        assert!(McpClient::connect(&c, "evil").is_err());
        // The refusal is journaled honestly (audit trail intact).
        let tail = c.journal().tail(20).unwrap();
        assert!(
            tail.iter().any(|e| e.str_of("kind") == "policy.decision"
                && !e.get("data").map(|d| d.b_of("allowed")).unwrap_or(true)),
            "add denial should be journaled as a denied policy.decision"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    // ---------- add/remove tests ----------

    #[test]
    fn add_server_round_trips_through_list_servers() {
        let (base, c) = temp_ctx();
        add_server(
            &c,
            "my-srv",
            "npx",
            &["-y".to_string(), "mcp-server-foo".to_string()],
        )
        .unwrap();
        let servers = list_servers(&c);
        let entry = servers.iter().find(|(n, _)| n == "my-srv");
        assert!(entry.is_some(), "server 'my-srv' should appear in list");
        let (_, cfg) = entry.unwrap();
        assert_eq!(cfg.str_of("command"), "npx");
        let args = cfg.strs_of("args");
        assert_eq!(args, vec!["-y", "mcp-server-foo"]);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn add_server_creates_mcp_json_when_absent() {
        let (base, c) = temp_ctx();
        assert!(
            !c.mcp_path().exists(),
            "mcp.json should not exist before add"
        );
        add_server(&c, "new-srv", "uvx", &["mcp-server-time".to_string()]).unwrap();
        assert!(c.mcp_path().exists(), "mcp.json should be created by add");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn duplicate_add_errors() {
        let (base, c) = temp_ctx();
        add_server(&c, "dup", "uvx", &[]).unwrap();
        let err = add_server(&c, "dup", "uvx", &[]).unwrap_err();
        assert!(
            err.contains("already exists"),
            "duplicate add should mention 'already exists', got: {err}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_missing_server_errors() {
        let (base, c) = temp_ctx();
        let err = remove_server(&c, "ghost").unwrap_err();
        assert!(
            err.contains("not found"),
            "remove of missing server should mention 'not found', got: {err}"
        );
        // Also errors when mcp.json exists but server is absent.
        add_server(&c, "real", "uvx", &[]).unwrap();
        let err2 = remove_server(&c, "ghost").unwrap_err();
        assert!(err2.contains("not found"), "got: {err2}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn add_and_remove_both_journal() {
        let (base, c) = temp_ctx();
        add_server(&c, "jrn-srv", "uvx", &["--flag".to_string()]).unwrap();
        remove_server(&c, "jrn-srv").unwrap();
        let tail = c.journal().tail(20).unwrap();
        assert!(
            tail.iter().any(|e| e.str_of("kind") == "mcp.add"),
            "journal should contain mcp.add"
        );
        assert!(
            tail.iter().any(|e| e.str_of("kind") == "mcp.remove"),
            "journal should contain mcp.remove"
        );
        std::fs::remove_dir_all(&base).ok();
    }
    #[test]
    fn mcp_add_denied_command_refused_and_journaled_not_persisted() {
        // F1 (CRITICAL): a command outside mcp_allow_commands must be refused
        // at add time, journal a denied policy.decision, and NOT land in
        // mcp.json (the audit trail must not show a clean mcp.add).
        let (base, c) = temp_ctx();
        let err = add_server(&c, "evil", "sudo", &["rm".to_string(), "-rf".to_string()])
            .unwrap_err();
        assert!(err.contains("blocked by policy"), "got: {err}");
        // Not persisted.
        assert!(list_servers(&c).iter().all(|(name, _)| name != "evil"));
        // Journaled as a denial, not as mcp.add.
        let tail = c.journal().tail(10).unwrap();
        assert!(tail.iter().all(|e| e.str_of("kind") != "mcp.add"));
        assert!(tail.iter().any(|e| e.str_of("kind") == "policy.decision"
            && e.get("data").map(|d| !d.b_of("allowed")).unwrap_or(false)));
        std::fs::remove_dir_all(&base).ok();
    }
}
