//! R3 — the policy engine: the operator's contract with the runtime.
//!
//! Policies live in `.soma/policy.json`, are plain JSON, and every decision
//! made against them is journaled with the rule that fired — so an audit can
//! answer "why was this allowed?" as well as "what happened?".

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::util::*;
use std::path::Path;

pub const AUTONOMY_LEVELS: [&str; 3] = ["observe", "assist", "auto"];

#[derive(Debug, Clone)]
pub struct Policy {
    /// observe = plan/select/explain only; assist = execute, human applies
    /// proposals; auto = execute + auto-apply mechanical proposals on tick.
    pub autonomy: String,
    pub allow_commands: Vec<String>,
    pub deny_commands: Vec<String>,
    /// Stricter allowlist for the *binary* an MCP server spawns. Connecting a
    /// server executes code and is reachable from the cockpit webview, so the
    /// launcher must be named explicitly here — `allow_commands` being
    /// permissive must not, by itself, let `mcp add --cmd /bin/sh` run.
    pub mcp_allow_commands: Vec<String>,
    pub allow_network: bool,
    pub allow_hosts: Vec<String>,
    pub writable_paths: Vec<String>,
    pub redact_keys: Vec<String>,
    pub max_timeout_s: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow { rule: String },
    Deny { rule: String },
}

impl Decision {
    pub fn allowed(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }
    pub fn rule(&self) -> &str {
        match self {
            Decision::Allow { rule } | Decision::Deny { rule } => rule,
        }
    }
    pub fn to_json(&self, subject: &str) -> Json {
        jobj(vec![
            ("subject", jstr(subject)),
            ("allowed", jbool(self.allowed())),
            ("rule", jstr(self.rule())),
        ])
    }
}

impl Policy {
    pub fn default_policy() -> Policy {
        Policy {
            autonomy: "assist".into(),
            allow_commands: vec!["*".into()],
            deny_commands: vec![
                "*rm -rf /*".into(),
                "sudo *".into(),
                "*mkfs*".into(),
                "*shutdown*".into(),
                "*reboot*".into(),
                "*dd if=*of=/dev/*".into(),
                "*:(){ :|:& };:*".into(),
            ],
            // Launchers the connector catalog actually uses. Notably absent:
            // sh/bash/zsh and bare interpreters (python3/node) — those turn
            // `--cmd <x> --arg -c --arg '<payload>'` into direct execution.
            mcp_allow_commands: vec![
                "npx".into(),
                "uvx".into(),
                "bunx".into(),
                "deno".into(),
                "memora-cli".into(),
            ],
            allow_network: false,
            allow_hosts: vec!["127.0.0.1".into(), "localhost".into()],
            writable_paths: vec!["{project}/*".into(), "/tmp/*".into(), "/private/tmp/*".into()],
            redact_keys: vec![
                "*key*".into(),
                "*token*".into(),
                "*secret*".into(),
                "*password*".into(),
                "*credential*".into(),
                // F6: `Authorization:`/`auth=` and `Bearer`-keyed values are
                // common secret carriers in child output and request dumps.
                "*auth*".into(),
                "*bearer*".into(),
            ],
            max_timeout_s: 600,
        }
    }

    pub fn to_json(&self) -> Json {
        jobj(vec![
            ("autonomy", jstr(&self.autonomy)),
            (
                "allow_commands",
                jarr(self.allow_commands.iter().map(|s| jstr(s)).collect()),
            ),
            (
                "deny_commands",
                jarr(self.deny_commands.iter().map(|s| jstr(s)).collect()),
            ),
            (
                "mcp_allow_commands",
                jarr(self.mcp_allow_commands.iter().map(|s| jstr(s)).collect()),
            ),
            ("allow_network", jbool(self.allow_network)),
            (
                "allow_hosts",
                jarr(self.allow_hosts.iter().map(|s| jstr(s)).collect()),
            ),
            (
                "writable_paths",
                jarr(self.writable_paths.iter().map(|s| jstr(s)).collect()),
            ),
            (
                "redact_keys",
                jarr(self.redact_keys.iter().map(|s| jstr(s)).collect()),
            ),
            ("max_timeout_s", jint(self.max_timeout_s)),
        ])
    }

    /// Build a Policy from JSON, failing CLOSED on present-but-wrong-type
    /// fields. A field that is ABSENT uses its default (so `soma init`'s
    /// partial/empty policy and a fresh project keep working). A field that is
    /// PRESENT but the wrong JSON type is a corrupt operator contract: rather
    /// than silently coercing it to an empty Vec / a default (which would
    /// *widen* the command or write gate, or disable secret redaction), we
    /// refuse, naming the offending field. `load()` propagates the Err.
    pub fn from_json(j: &Json) -> R<Policy> {
        let d = Policy::default_policy();

        // autonomy: absent → default; wrong type or unknown value → refuse.
        // (Don't pick a more-permissive-than-observe default for a corrupt
        // contract — an invalid level fails closed.)
        let autonomy = match j.get("autonomy") {
            None => d.autonomy.clone(),
            Some(v) => {
                let a = v.s().ok_or_else(|| {
                    "policy field 'autonomy' is present but not a string — refusing to run (fail closed)".to_string()
                })?;
                if AUTONOMY_LEVELS.contains(&a) {
                    a.to_string()
                } else {
                    return Err(format!(
                        "policy field 'autonomy' has invalid value {a:?} (must be one of {AUTONOMY_LEVELS:?}) — refusing to run (fail closed)"
                    ));
                }
            }
        };

        // String-array fields: absent → default; present-but-not-an-array, or
        // an array containing a non-string element → refuse.
        let strs = |key: &str, def: &Vec<String>| -> R<Vec<String>> {
            match j.get(key) {
                None => Ok(def.clone()),
                Some(v) => {
                    let arr = v.arr().ok_or_else(|| {
                        format!("policy field '{key}' is present but not an array — refusing to run (fail closed)")
                    })?;
                    arr.iter()
                        .map(|item| {
                            item.s().map(|s| s.to_string()).ok_or_else(|| {
                                format!("policy field '{key}' contains a non-string element — refusing to run (fail closed)")
                            })
                        })
                        .collect()
                }
            }
        };

        // bool field: absent → default; present-but-not-a-bool → refuse.
        let allow_network = match j.get("allow_network") {
            None => d.allow_network,
            Some(v) => v.b().ok_or_else(|| {
                "policy field 'allow_network' is present but not a boolean — refusing to run (fail closed)".to_string()
            })?,
        };

        // int field: absent → default; present-but-not-an-int → refuse.
        // (A non-positive int keeps the default timeout — that's a value
        // clamp, not a type error.)
        let max_timeout_s = match j.get("max_timeout_s") {
            None => d.max_timeout_s,
            Some(v) => {
                let t = v.i().ok_or_else(|| {
                    "policy field 'max_timeout_s' is present but not an integer — refusing to run (fail closed)".to_string()
                })?;
                if t > 0 {
                    t
                } else {
                    d.max_timeout_s
                }
            }
        };

        Ok(Policy {
            autonomy,
            allow_commands: strs("allow_commands", &d.allow_commands)?,
            deny_commands: strs("deny_commands", &d.deny_commands)?,
            mcp_allow_commands: strs("mcp_allow_commands", &d.mcp_allow_commands)?,
            allow_network,
            allow_hosts: strs("allow_hosts", &d.allow_hosts)?,
            writable_paths: strs("writable_paths", &d.writable_paths)?,
            redact_keys: strs("redact_keys", &d.redact_keys)?,
            max_timeout_s,
        })
    }

    /// Load the policy, failing CLOSED at the policy root.
    ///
    /// A missing file means a fresh project → defaults (`soma init` keeps
    /// working). A file that is PRESENT but unreadable, unparseable, or not
    /// a JSON object is a hard error: the default policy is permissive on
    /// commands (`allow_commands: ["*"]`), so silently falling back would
    /// *widen* the command gate exactly when the operator's contract is
    /// corrupt. The runtime refuses to proceed far enough to act.
    pub fn load(path: &Path) -> R<Policy> {
        if !path.exists() {
            return Ok(Policy::default_policy());
        }
        let s = read_to_string(path)
            .map_err(|e| format!("policy file {} exists but cannot be read ({e}) — refusing to run", path.display()))?;
        let j = crate::json::parse(&s).map_err(|e| {
            format!(
                "policy file {} is unparseable JSON ({e}) — refusing to run (fail closed); fix the file or delete it to restore defaults",
                path.display()
            )
        })?;
        if j.obj().is_none() {
            return Err(format!(
                "policy file {} is not a JSON object — refusing to run (fail closed); fix the file or delete it to restore defaults",
                path.display()
            ));
        }
        // A present-but-wrong-type field (e.g. `"deny_commands":"rm -rf /"`)
        // must not silently become an empty Vec — that WIDENS the gate. Name
        // the file so the operator can fix it.
        Policy::from_json(&j).map_err(|e| {
            format!(
                "policy file {} is invalid: {e}; fix the file or delete it to restore defaults",
                path.display()
            )
        })
    }

    pub fn save(&self, path: &Path) -> R<()> {
        atomic_write(path, self.to_json().pretty().as_bytes())
    }

    /// Gate a shell command: deny patterns win, then the allow list must match.
    pub fn check_command(&self, cmd: &str) -> Decision {
        for pat in &self.deny_commands {
            if glob_match(pat, cmd) {
                return Decision::Deny {
                    rule: format!("deny_commands:{pat}"),
                };
            }
        }
        for pat in &self.allow_commands {
            if glob_match(pat, cmd) {
                return Decision::Allow {
                    rule: format!("allow_commands:{pat}"),
                };
            }
        }
        Decision::Deny {
            rule: "allow_commands:no-match".into(),
        }
    }

    /// Gate the *binary* an MCP server would spawn — stricter than the general
    /// `allow_commands` glob. Matched against both the command as written and
    /// its basename, so an absolute path to an allowed launcher (e.g.
    /// `/usr/local/bin/npx`) still passes while `/bin/sh` does not.
    pub fn check_mcp_command(&self, command: &str) -> Decision {
        let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
        for pat in &self.mcp_allow_commands {
            if pat == command || pat == base || glob_match(pat, command) || glob_match(pat, base) {
                return Decision::Allow {
                    rule: format!("mcp_allow_commands:{pat}"),
                };
            }
        }
        Decision::Deny {
            rule: "mcp_allow_commands:no-match".into(),
        }
    }

    /// Gate execution (skill/goal/cron runs) on autonomy level.
    pub fn check_execution(&self, what: &str) -> Decision {
        if self.autonomy == "observe" {
            Decision::Deny {
                rule: format!("autonomy:observe blocks {what}"),
            }
        } else {
            Decision::Allow {
                rule: format!("autonomy:{}", self.autonomy),
            }
        }
    }

    /// Gate outbound network by host.
    pub fn check_network(&self, host: &str) -> Decision {
        if !self.allow_network
            && !self
                .allow_hosts
                .iter()
                .any(|h| h == host || glob_match(h, host))
        {
            return Decision::Deny {
                rule: "allow_network:false".into(),
            };
        }
        if self.allow_hosts.iter().any(|h| h == host || glob_match(h, host)) {
            return Decision::Allow {
                rule: format!("allow_hosts:{host}"),
            };
        }
        if self.allow_network {
            Decision::Allow {
                rule: "allow_network:true".into(),
            }
        } else {
            Decision::Deny {
                rule: "allow_network:false".into(),
            }
        }
    }

    /// Gate writes outside the sanctioned roots. `{project}` expands to the
    /// project root.
    pub fn check_path_write(&self, path: &str, project_root: &str) -> Decision {
        for pat in &self.writable_paths {
            let pat = pat.replace("{project}", project_root);
            if glob_match(&pat, path) || pat.trim_end_matches('/') == path {
                return Decision::Allow {
                    rule: format!("writable_paths:{pat}"),
                };
            }
        }
        Decision::Deny {
            rule: "writable_paths:no-match".into(),
        }
    }

    /// May proposals be applied without a human? Only in `auto`.
    pub fn auto_apply_allowed(&self) -> bool {
        self.autonomy == "auto"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_block_destructive_commands() {
        let p = Policy::default_policy();
        assert!(!p.check_command("sudo rm -rf /").allowed());
        assert!(!p.check_command("rm -rf / --no-preserve-root").allowed());
        assert!(p.check_command("cargo build --release").allowed());
        assert!(p.check_command("echo hello").allowed());
    }

    #[test]
    fn mcp_allowlist_blocks_shell_injection() {
        let p = Policy::default_policy();
        // Catalog launchers pass, by bare name or absolute path.
        assert!(p.check_mcp_command("npx").allowed());
        assert!(p.check_mcp_command("uvx").allowed());
        assert!(p.check_mcp_command("/usr/local/bin/npx").allowed());
        assert!(p.check_mcp_command("memora-cli").allowed());
        // The reported RCE vectors are denied even though allow_commands is "*".
        assert!(!p.check_mcp_command("/bin/sh").allowed());
        assert!(!p.check_mcp_command("bash").allowed());
        assert!(!p.check_mcp_command("python3").allowed());
        assert!(!p.check_mcp_command("node").allowed());
        assert_eq!(
            p.check_mcp_command("/bin/sh").rule(),
            "mcp_allow_commands:no-match"
        );
    }

    #[test]
    fn deny_wins_over_allow() {
        let mut p = Policy::default_policy();
        p.allow_commands = vec!["*".into()];
        p.deny_commands = vec!["*git push*".into()];
        let d = p.check_command("git push origin main");
        assert!(!d.allowed());
        assert!(d.rule().contains("deny_commands"));
    }

    #[test]
    fn empty_allow_list_denies_everything() {
        let mut p = Policy::default_policy();
        p.allow_commands = vec![];
        assert!(!p.check_command("echo hi").allowed());
    }

    #[test]
    fn autonomy_gates_execution() {
        let mut p = Policy::default_policy();
        p.autonomy = "observe".into();
        assert!(!p.check_execution("skill.run").allowed());
        p.autonomy = "assist".into();
        assert!(p.check_execution("skill.run").allowed());
        assert!(!p.auto_apply_allowed());
        p.autonomy = "auto".into();
        assert!(p.auto_apply_allowed());
    }

    #[test]
    fn network_default_localhost_only() {
        let p = Policy::default_policy();
        assert!(p.check_network("127.0.0.1").allowed());
        assert!(p.check_network("localhost").allowed());
        assert!(!p.check_network("api.anthropic.com").allowed());
        let mut open = p.clone();
        open.allow_network = true;
        assert!(open.check_network("api.anthropic.com").allowed());
    }

    #[test]
    fn path_boundaries() {
        let p = Policy::default_policy();
        assert!(p
            .check_path_write("/home/me/proj/out.txt", "/home/me/proj")
            .allowed());
        assert!(!p.check_path_write("/etc/passwd", "/home/me/proj").allowed());
        assert!(p.check_path_write("/tmp/x", "/home/me/proj").allowed());
    }

    #[test]
    fn load_missing_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("soma-pol-{}", new_id("t")));
        ensure_dir(&dir).unwrap();
        let p = Policy::load(&dir.join("policy.json")).expect("missing file must give defaults");
        let d = Policy::default_policy();
        assert_eq!(p.autonomy, d.autonomy);
        assert_eq!(p.allow_commands, d.allow_commands);
        assert_eq!(p.allow_network, d.allow_network);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_unparseable_file_fails_closed() {
        let dir = std::env::temp_dir().join(format!("soma-pol-{}", new_id("t")));
        ensure_dir(&dir).unwrap();
        let path = dir.join("policy.json");

        // Corrupt JSON → hard error naming the file and the parse problem.
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = Policy::load(&path).expect_err("unparseable policy must refuse");
        assert!(err.contains("policy.json"), "error must name the file: {err}");
        assert!(err.contains("unparseable"), "error must state the problem: {err}");
        assert!(err.contains("fail closed"), "error must say it fails closed: {err}");

        // Parseable but not an object → also refused (a JSON array or string
        // is not a policy; from_json would silently produce all-defaults).
        std::fs::write(&path, "[1,2,3]").unwrap();
        let err = Policy::load(&path).expect_err("non-object policy must refuse");
        assert!(err.contains("not a JSON object"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_valid_file_roundtrips() {
        let dir = std::env::temp_dir().join(format!("soma-pol-{}", new_id("t")));
        ensure_dir(&dir).unwrap();
        let path = dir.join("policy.json");
        let mut p = Policy::default_policy();
        p.autonomy = "observe".into();
        p.allow_hosts.push("freetsa.org".into());
        p.save(&path).unwrap();
        let q = Policy::load(&path).expect("valid file must load");
        assert_eq!(q.autonomy, "observe");
        assert!(q.allow_hosts.iter().any(|h| h == "freetsa.org"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn json_roundtrip() {
        let p = Policy::default_policy();
        let q = Policy::from_json(&p.to_json()).expect("valid policy roundtrips");
        assert_eq!(p.autonomy, q.autonomy);
        assert_eq!(p.deny_commands, q.deny_commands);
        assert_eq!(p.allow_network, q.allow_network);
        // An invalid autonomy now FAILS CLOSED (no silent fallback to assist):
        // a corrupt contract must not pick a more-permissive default.
        let bad = crate::json::parse(r#"{"autonomy":"yolo"}"#).unwrap();
        let err = Policy::from_json(&bad).expect_err("invalid autonomy must refuse");
        assert!(err.contains("autonomy"), "{err}");
    }

    #[test]
    fn wrong_typed_array_field_fails_closed_not_empty() {
        // A string where deny_commands expects an array MUST refuse — silently
        // becoming [] would widen the command gate (allow stays ["*"]).
        let bad = crate::json::parse(r#"{"deny_commands":"rm -rf /"}"#).unwrap();
        let err = Policy::from_json(&bad).expect_err("string deny_commands must refuse");
        assert!(err.contains("deny_commands"), "must name the field: {err}");
        assert!(err.contains("not an array"), "{err}");

        // Same for the other security-bearing string-array fields.
        for key in ["allow_commands", "mcp_allow_commands", "allow_hosts", "writable_paths", "redact_keys"] {
            let j = crate::json::parse(&format!("{{\"{key}\":\"x\"}}")).unwrap();
            let err = Policy::from_json(&j).expect_err("wrong-typed array must refuse");
            assert!(err.contains(key), "must name {key}: {err}");
        }

        // An array with a non-string element also refuses (no silent drop).
        let mixed = crate::json::parse(r#"{"allow_hosts":["ok",42]}"#).unwrap();
        let err = Policy::from_json(&mixed).expect_err("non-string element must refuse");
        assert!(err.contains("allow_hosts"), "{err}");
        assert!(err.contains("non-string"), "{err}");
    }

    #[test]
    fn wrong_typed_bool_and_int_fail_closed() {
        // allow_network given a string (not a bool) → refuse, not default-false.
        let bad = crate::json::parse(r#"{"allow_network":"true"}"#).unwrap();
        let err = Policy::from_json(&bad).expect_err("string allow_network must refuse");
        assert!(err.contains("allow_network"), "{err}");
        assert!(err.contains("not a boolean"), "{err}");

        // max_timeout_s given a string → refuse.
        let bad = crate::json::parse(r#"{"max_timeout_s":"600"}"#).unwrap();
        let err = Policy::from_json(&bad).expect_err("string max_timeout_s must refuse");
        assert!(err.contains("max_timeout_s"), "{err}");
    }

    #[test]
    fn valid_and_partial_policies_still_load_defaults() {
        // A MISSING-field policy keeps `soma init`'s partial files working:
        // absent fields take defaults, present ones override.
        let partial = crate::json::parse(r#"{"autonomy":"observe"}"#).unwrap();
        let p = Policy::from_json(&partial).expect("partial policy must load");
        let d = Policy::default_policy();
        assert_eq!(p.autonomy, "observe");
        assert_eq!(p.deny_commands, d.deny_commands); // default kept
        assert_eq!(p.allow_commands, d.allow_commands);
        assert_eq!(p.allow_network, d.allow_network);

        // An empty object is all-defaults (the init path).
        let empty = crate::json::parse("{}").unwrap();
        let p = Policy::from_json(&empty).expect("empty object must load defaults");
        assert_eq!(p.autonomy, d.autonomy);
        assert_eq!(p.deny_commands, d.deny_commands);

        // A full valid policy roundtrips intact.
        let full = Policy::default_policy().to_json();
        let p = Policy::from_json(&full).expect("full policy must load");
        assert_eq!(p.allow_hosts, d.allow_hosts);
        assert_eq!(p.redact_keys, d.redact_keys);
    }

    #[test]
    fn load_wrong_typed_field_refuses_at_file_level() {
        // End-to-end through load(): a wrong-typed field on disk makes the
        // runtime refuse rather than running with a widened gate. This is the
        // reproduction from the verification sweep.
        let dir = std::env::temp_dir().join(format!("soma-pol-{}", new_id("t")));
        ensure_dir(&dir).unwrap();
        let path = dir.join("policy.json");
        std::fs::write(&path, r#"{"deny_commands":"rm -rf /"}"#).unwrap();
        let err = Policy::load(&path).expect_err("wrong-typed deny_commands must refuse to load");
        assert!(err.contains("policy.json"), "must name the file: {err}");
        assert!(err.contains("deny_commands"), "must name the field: {err}");
        assert!(err.contains("fail closed"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
