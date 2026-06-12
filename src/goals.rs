//! R12 — goals and workflows: directed, verified, journaled execution.
//!
//! A goal carries a why and acceptance criteria; its workflow is an ordered
//! list of steps. Each step is a skill, a routed model call, or a raw command,
//! with its own verification (exit0 | contains | command). Storage is
//! append-only JSONL — updating a goal appends a new record with the same id;
//! the latest record wins. The journal gets one event per step, so an export
//! shows not just *that* a goal ran but *how*.

use crate::json::{jarr, jbool, jint, jobj, jstr, Json};
use crate::project::Ctx;
use crate::skills;
use crate::util::*;

pub const STEP_KINDS: [&str; 3] = ["skill", "model", "command"];
pub const VERIFY_KINDS: [&str; 3] = ["exit0", "contains", "command"];

fn append_goal(c: &Ctx, goal: &Json) -> R<()> {
    append_line(&c.goals_path(), &goal.to_string())
}

/// Latest record per goal id, in first-seen order.
pub fn list(c: &Ctx) -> R<Vec<Json>> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, Json> = std::collections::HashMap::new();
    for_each_line(&c.goals_path(), |line| {
        if let Ok(g) = crate::json::parse(line) {
            let id = g.str_of("id");
            if !id.is_empty() {
                if !latest.contains_key(&id) {
                    order.push(id.clone());
                }
                latest.insert(id, g);
            }
        }
        Ok(())
    })?;
    Ok(order.into_iter().filter_map(|id| latest.remove(&id)).collect())
}

pub fn get(c: &Ctx, id: &str) -> R<Json> {
    list(c)?
        .into_iter()
        .find(|g| g.str_of("id") == id || g.str_of("title") == id)
        .ok_or_else(|| format!("no goal '{id}' (see `soma goal list`)"))
}

pub fn add(c: &Ctx, title: &str, why: &str, acceptance: &[String]) -> R<Json> {
    if title.trim().is_empty() {
        return Err("goal title required".into());
    }
    let ts = now_ms();
    let goal = jobj(vec![
        ("id", jstr(new_id("gl"))),
        ("ts", jint(ts)),
        ("iso", jstr(&iso8601(ts))),
        ("title", jstr(title)),
        ("why", jstr(why)),
        (
            "acceptance",
            jarr(acceptance.iter().map(|a| jstr(a)).collect()),
        ),
        ("steps", jarr(vec![])),
        ("status", jstr("open")),
    ]);
    append_goal(c, &goal)?;
    c.log(
        "goal.add",
        jobj(vec![("id", jstr(&goal.str_of("id"))), ("title", jstr(title))]),
    )?;
    Ok(goal)
}

/// Validate and append one step to a goal's workflow.
pub fn add_step(c: &Ctx, goal_id: &str, step: Json) -> R<Json> {
    let kind = step.str_of("kind");
    if !STEP_KINDS.contains(&kind.as_str()) {
        return Err(format!("step.kind must be one of {STEP_KINDS:?}"));
    }
    if step.str_of("name").is_empty() {
        return Err("step.name required".into());
    }
    if kind != "skill" && step.str_of("input").is_empty() {
        return Err("step.input required (the prompt / command line)".into());
    }
    if kind == "skill" && step.str_of("skill").is_empty() {
        return Err("step.skill required for skill steps".into());
    }
    if let Some(v) = step.get("verify") {
        let vk = v.str_of("kind");
        if !VERIFY_KINDS.contains(&vk.as_str()) {
            return Err(format!("verify.kind must be one of {VERIFY_KINDS:?}"));
        }
        if vk != "exit0" && v.str_of("value").is_empty() {
            return Err("verify.value required for contains/command verification".into());
        }
    }
    let mut goal = get(c, goal_id)?;
    let mut steps = goal.arr_of("steps");
    steps.push(step.clone());
    goal.set("steps", Json::Arr(steps));
    append_goal(c, &goal)?;
    c.log(
        "goal.step.add",
        jobj(vec![
            ("goal", jstr(&goal.str_of("id"))),
            ("step", jstr(&step.str_of("name"))),
            ("kind", jstr(&kind)),
        ]),
    )?;
    Ok(goal)
}

#[derive(Debug)]
pub struct StepResult {
    pub name: String,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug)]
pub struct RunReport {
    pub goal_id: String,
    pub title: String,
    pub steps: Vec<StepResult>,
    pub ok: bool,
    pub acceptance: Vec<String>,
}

fn run_step(c: &Ctx, step: &Json) -> R<(bool, String, String)> {
    // → (ok-by-execution, primary_output, note)
    let input = step.str_of("input");
    match step.str_of("kind").as_str() {
        "skill" => {
            let s = skills::find(c, &step.str_of("skill"))?;
            // step.input only feeds skills that declare an {input} placeholder;
            // otherwise it's descriptive and must not leak onto the command line
            let pass_input = !input.is_empty() && s.cmd().contains("{input}");
            let out = skills::run(c, &s, if pass_input { Some(&input) } else { None })?;
            let note = format!("skill {} → {} ({}ms)", s.name(), if out.ok { "ok" } else { "fail" }, out.ms);
            Ok((out.ok, out.stdout, note))
        }
        "model" => {
            let (route, reply) = crate::models::ask_routed(c, &input)?;
            let note = format!(
                "model {}:{} ({}, {}ms{})",
                reply.provider,
                reply.model,
                route.level,
                reply.ms,
                if reply.cached { ", cached" } else { "" }
            );
            Ok((!reply.text.is_empty(), reply.text, note))
        }
        "command" => {
            let res = skills::exec_shell(c, &input, 300)?;
            let note = format!("command exit {} ({}ms)", res.exit_code, res.ms);
            Ok((res.exit_code == 0 && !res.timed_out, res.stdout, note))
        }
        other => Err(format!("unknown step kind '{other}'")),
    }
}

fn verify_step(c: &Ctx, step: &Json, exec_ok: bool, output: &str) -> R<(bool, String)> {
    match step.get("verify") {
        None => Ok((exec_ok, "default: execution succeeded".into())),
        Some(v) => match v.str_of("kind").as_str() {
            "exit0" => Ok((exec_ok, "verify exit0".into())),
            "contains" => {
                let needle = v.str_of("value");
                let hit = output.contains(&needle);
                Ok((hit, format!("verify output contains '{needle}': {hit}")))
            }
            "command" => {
                let cmd = v.str_of("value");
                let res = skills::exec_shell(c, &cmd, 120)?;
                Ok((
                    res.exit_code == 0 && !res.timed_out,
                    format!("verify command `{}` exit {}", truncate_chars(&cmd, 60), res.exit_code),
                ))
            }
            other => Err(format!("unknown verify kind '{other}'")),
        },
    }
}

/// Execute a goal's workflow through the policy gate. Each step and its
/// verification is journaled; the goal record is updated with the outcome.
pub fn run(c: &Ctx, id: &str, halt_on_fail: bool) -> R<RunReport> {
    let mut goal = get(c, id)?;
    let goal_id = goal.str_of("id");
    let exec = c.policy.check_execution("goal.run");
    c.log("policy.decision", exec.to_json(&format!("goal.run:{goal_id}")))?;
    if !exec.allowed() {
        return Err(format!("blocked by policy ({})", exec.rule()));
    }
    let steps = goal.arr_of("steps");
    if steps.is_empty() {
        return Err("goal has no steps — add some with `soma goal step`".into());
    }
    c.log(
        "goal.run",
        jobj(vec![
            ("goal", jstr(&goal_id)),
            ("title", jstr(&goal.str_of("title"))),
            ("steps", jint(steps.len() as i64)),
        ]),
    )?;

    let mut results: Vec<StepResult> = Vec::new();
    let mut all_ok = true;
    for step in &steps {
        let name = step.str_of("name");
        let (ok, note) = match run_step(c, step) {
            Ok((exec_ok, output, exec_note)) => {
                let (vok, vnote) = verify_step(c, step, exec_ok, &output)?;
                (vok, format!("{exec_note}; {vnote}"))
            }
            Err(e) => (false, format!("step error: {e}")),
        };
        c.log(
            "goal.step",
            jobj(vec![
                ("goal", jstr(&goal_id)),
                ("step", jstr(&name)),
                ("ok", jbool(ok)),
                ("note", jstr(&truncate_chars(&note, 200))),
            ]),
        )?;
        results.push(StepResult {
            name,
            ok,
            note,
        });
        if !ok {
            all_ok = false;
            if halt_on_fail {
                break;
            }
        }
    }

    let status = if all_ok { "done" } else { "failed" };
    goal.set("status", jstr(status));
    goal.set("last_run_iso", jstr(&iso8601(now_ms())));
    goal.set(
        "last_run",
        jarr(
            results
                .iter()
                .map(|r| {
                    jobj(vec![
                        ("step", jstr(&r.name)),
                        ("ok", jbool(r.ok)),
                        ("note", jstr(&truncate_chars(&r.note, 200))),
                    ])
                })
                .collect(),
        ),
    );
    append_goal(c, &goal)?;
    c.log(
        "goal.done",
        jobj(vec![
            ("goal", jstr(&goal_id)),
            ("status", jstr(status)),
            (
                "steps_ok",
                jint(results.iter().filter(|r| r.ok).count() as i64),
            ),
            ("steps_total", jint(results.len() as i64)),
        ]),
    )?;
    Ok(RunReport {
        goal_id,
        title: goal.str_of("title"),
        steps: results,
        ok: all_ok,
        acceptance: goal.strs_of("acceptance"),
    })
}

pub fn render_report(r: &RunReport) -> String {
    let mut out = format!(
        "goal: {} — {}\n",
        r.title,
        if r.ok { "DONE ✓" } else { "FAILED ✗" }
    );
    for s in &r.steps {
        out.push_str(&format!(
            "  [{}] {} — {}\n",
            if s.ok { "ok" } else { "FAIL" },
            s.name,
            s.note
        ));
    }
    if !r.acceptance.is_empty() {
        out.push_str("  acceptance criteria (review against the output above):\n");
        for a in &r.acceptance {
            out.push_str(&format!("    - {a}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testutil::temp_ctx;

    fn step(name: &str, kind: &str, input: &str, verify: Option<(&str, &str)>) -> Json {
        let mut s = jobj(vec![
            ("name", jstr(name)),
            ("kind", jstr(kind)),
            ("input", jstr(input)),
        ]);
        if kind == "skill" {
            s.set("skill", jstr(input));
            s.set("input", jstr(""));
            // for skill steps the test passes skill name via input arg
        }
        if let Some((k, v)) = verify {
            s.set("verify", jobj(vec![("kind", jstr(k)), ("value", jstr(v))]));
        }
        s
    }

    #[test]
    fn add_run_verify_roundtrip() {
        let (base, c) = temp_ctx();
        let g = add(&c, "ship it", "prove the workflow engine", &["echo works".into()]).unwrap();
        let id = g.str_of("id");
        add_step(&c, &id, step("greet", "command", "echo hello-world", Some(("contains", "hello-world")))).unwrap();
        add_step(&c, &id, step("count", "command", "ls", None)).unwrap();
        let rep = run(&c, &id, true).unwrap();
        assert!(rep.ok, "{rep:?}");
        assert_eq!(rep.steps.len(), 2);
        // goal status updated to done
        assert_eq!(get(&c, &id).unwrap().str_of("status"), "done");
        // journal carries per-step events
        let tail = c.journal().tail(20).unwrap();
        assert!(tail.iter().filter(|e| e.str_of("kind") == "goal.step").count() >= 2);
        assert!(tail.iter().any(|e| e.str_of("kind") == "goal.done"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn failing_verification_halts_and_marks_failed() {
        let (base, c) = temp_ctx();
        let g = add(&c, "doomed", "verification must catch lies", &[]).unwrap();
        let id = g.str_of("id");
        add_step(&c, &id, step("lie", "command", "echo nope", Some(("contains", "expected-token")))).unwrap();
        add_step(&c, &id, step("never-runs", "command", "echo unreachable", None)).unwrap();
        let rep = run(&c, &id, true).unwrap();
        assert!(!rep.ok);
        assert_eq!(rep.steps.len(), 1, "halt_on_fail should stop after first failure");
        assert_eq!(get(&c, &id).unwrap().str_of("status"), "failed");
        // continue-mode runs everything
        let rep2 = run(&c, &id, false).unwrap();
        assert_eq!(rep2.steps.len(), 2);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn model_step_routes_through_echo() {
        let (base, mut c) = temp_ctx();
        // route everything to the echo provider for an offline test
        let route = |m: &str| jobj(vec![("provider", jstr("echo")), ("model", jstr(m))]);
        let mut model = c.config.get("model").cloned().unwrap();
        model.set(
            "routing",
            jobj(vec![
                ("simple", route("s")),
                ("moderate", route("m")),
                ("complex", route("c")),
            ]),
        );
        c.config.set("model", model);
        c.save_config().unwrap();
        let c = crate::project::Ctx::load(Some(&c.root.to_string_lossy())).unwrap();

        let g = add(&c, "ask", "model steps work", &[]).unwrap();
        let id = g.str_of("id");
        add_step(&c, &id, step("question", "model", "summarize the project", Some(("contains", "summarize")))).unwrap();
        let rep = run(&c, &id, true).unwrap();
        assert!(rep.ok, "{rep:?}");
        assert!(rep.steps[0].note.contains("model echo"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn skill_step_executes_registered_skill() {
        let (base, c) = temp_ctx();
        crate::skills::add(
            &c,
            jobj(vec![
                ("name", jstr("hello")),
                ("purpose", jstr("prints a fixed greeting")),
                ("goal", jstr("greeting printed")),
                ("kind", jstr("command")),
                ("run", jobj(vec![("cmd", jstr("echo hi-from-skill")), ("timeout_s", jint(10))])),
            ]),
            false,
        )
        .unwrap();
        let g = add(&c, "use skill", "skill steps work", &[]).unwrap();
        let id = g.str_of("id");
        let mut s = jobj(vec![
            ("name", jstr("call-skill")),
            ("kind", jstr("skill")),
            ("skill", jstr("hello")),
            ("input", jstr("-")),
            ("verify", jobj(vec![("kind", jstr("contains")), ("value", jstr("hi-from-skill"))])),
        ]);
        // skill takes no meaningful input; pass placeholder
        s.set("input", jstr(""));
        // empty input fails validation; set to something harmless
        s.set("input", jstr("unused"));
        add_step(&c, &id, s).unwrap();
        let rep = run(&c, &id, true).unwrap();
        assert!(rep.ok, "{rep:?}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn validation_rejects_bad_steps() {
        let (base, c) = temp_ctx();
        let g = add(&c, "strict", "validation works", &[]).unwrap();
        let id = g.str_of("id");
        assert!(add_step(&c, &id, jobj(vec![("name", jstr("x")), ("kind", jstr("teleport")), ("input", jstr("y"))])).is_err());
        assert!(add_step(&c, &id, jobj(vec![("kind", jstr("command")), ("input", jstr("ls"))])).is_err());
        assert!(add_step(
            &c,
            &id,
            jobj(vec![
                ("name", jstr("x")),
                ("kind", jstr("command")),
                ("input", jstr("ls")),
                ("verify", jobj(vec![("kind", jstr("contains")), ("value", jstr(""))])),
            ])
        )
        .is_err());
        assert!(add(&c, "  ", "", &[]).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn observe_mode_blocks_goal_run() {
        let (base, mut c) = temp_ctx();
        let g = add(&c, "blocked", "autonomy gate", &[]).unwrap();
        let id = g.str_of("id");
        add_step(&c, &id, step("s", "command", "echo x", None)).unwrap();
        c.policy.autonomy = "observe".into();
        assert!(run(&c, &id, true).unwrap_err().contains("policy"));
        std::fs::remove_dir_all(&base).ok();
    }
}
