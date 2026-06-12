# soma ↔ Agent Control Specification: a conformance mapping

**What this is.** A mapping of soma's runtime controls onto Microsoft's
Agent Control Specification (ACS), version `0.3.1-beta` (status: Draft),
plus an evaluation-feasibility note for ASSERT. Every claim below is
checkable against soma's source (`src/`) or the spec text.

**What this is not.** Not a certification - none exists; ACS conformance is
self-asserted against the spec and its in-repo test suite. Not an embedding
of the AGT policy engine: soma does not link, vendor, or shell out to
`agt_core_engine`. It is an independently built, zero-dependency runtime
whose controls happen to map onto the same decision model.

| | |
|---|---|
| Spec pinned | ACS `0.3.1-beta`, fetched 2026-06-12 from [`policy-engine/spec/SPECIFICATION.md`](https://github.com/microsoft/agent-governance-toolkit/blob/main/policy-engine/spec/SPECIFICATION.md) |
| AGT repo | <https://github.com/microsoft/agent-governance-toolkit> (MIT, Public Preview) |
| Announcement | [Build 2026 open trust stack - Foundry blog](https://devblogs.microsoft.com/foundry/build-2026-open-trust-stack-ai-agents/) |
| soma | v0.2.0, this repository. Zero crates; the supply chain is `src/`. |

## Why this matters

ACS is the embryo of a vendor-neutral standard for runtime agent
governance: a stateless, deterministic, fail-closed decision model with
named intervention points, a fixed verdict vocabulary, and content-safe
audit rules, open-sourced at Build 2026. soma was built independently of
it, but arrived at structurally similar answers - deny-by-default gates at
mutation boundaries, every decision journaled with the rule that fired. If
ACS becomes the lingua franca for "where may an agent be stopped, and what
may a verdict say", it is useful to state precisely where soma already
speaks it, and where it does not. This document does both.

A note on the spec's own status: the Foundry blog markets "five validation
checkpoints"; the normative spec defines **eight** intervention points
(Section 4). We cite the spec. The spec is also a Draft with rough edges - its Section 22 still says the version carries the `-alpha` pre-release tag
while the header declares `0.3.1-beta`. Expect breakage.

## The mapping

ACS evaluates one intervention point at a time and returns one of five
verdicts: `allow`, `warn`, `transform`, `deny`, `escalate` (Section 13).
soma's gates return two: allow or deny, each carrying the rule that fired
(`src/policy.rs`, `Decision`). The table maps each soma control to the ACS
point it would attach to, with verdict semantics and the reserved
`runtime_error:*` reason (Section 16) its failure path corresponds to.

| soma control | Source | ACS intervention point | Verdict semantics | Reserved-reason analogue |
|---|---|---|---|---|
| Command allow/deny globs (`check_command`) - gates skill, cron, goal-step, and proposal commands | `src/policy.rs`, called from `src/skills.rs`, `src/mcp.rs`, `src/improve.rs` | `pre_tool_call`, policy_target kind `tool_args` | deny-glob hit → `deny` (reason `deny_commands:<pat>`); allow-glob hit → `allow`; no match → `deny` (`allow_commands:no-match`) | - (policy-emitted reasons, as Section 13 permits) |
| `soma wrap` spawn gate (v6) - `check_command` over the full child command line before any process exists | `src/wrap.rs` | `pre_tool_call` over the launch (arguably `agent_startup` of the wrapped agent) | refusal exits non-zero, nothing spawned → `deny` | - |
| MCP two-gate: `check_mcp_command` at `mcp add` **and** again at spawn time | `src/mcp.rs` (both gates) | `agent_startup` (server process start) | strict allowlist; no match → `deny` (`mcp_allow_commands:no-match`) - `allow_commands: ["*"]` does not bypass it | `runtime_error:tool_unknown` is the nearest spirit: unknown launcher cannot run |
| Network host allowlist (`check_network`) - model-provider egress, v6 RFC 3161 anchor egress | `src/models.rs`, `src/anchor.rs` | `pre_tool_call`, host carried as an annotation / tool security label | host match → `allow` (`allow_hosts:<host>`); default → `deny` (`allow_network:false`); fresh projects are localhost-only | - |
| Path-write boundary (`check_path_write`) - where soma itself writes (e.g. `export --out`) | `src/export.rs` | `pre_tool_call`, policy_target = destination path | no matching root → `deny` (`writable_paths:no-match`) | - |
| Evidence-bundle finalization - `soma export` recomputes the full chain and **refuses** to export a broken one; v6 `soma anchor now` likewise refuses to anchor one | `src/export.rs`, `src/anchor.rs` | `agent_shutdown` (end-of-session audit gate) | broken chain → `deny`; no plausible-looking evidence of a tampered history | `runtime_error:manifest_invalid` in spirit: invalid state cannot proceed |
| Selector explanation (`select.explain` events) - deterministic scored skill choice with named factors | `src/neuro.rs` | `pre_model_call` **decision-event analogue only** | this is observability, not a gate; soma does not claim the `pre_model_call` point (see subset declaration) | - |

## Autonomy levels ↔ ACS modes and verdicts

ACS has two enforcement modes (Section 5) - `enforce` and `evaluate_only` - and routes `escalate` verdicts to a host approval path (Section 17.1).
soma's `autonomy` field maps as follows, with one honest wrinkle:

- **`auto` → `enforce`.** Gates bind; mechanical improvement proposals are
  auto-applied (an `allow` at the self-modification boundary).
- **`assist` → `enforce` + `escalate`-by-default for self-modification.**
  Command/network/path gates bind exactly as in `auto`, but applying an
  improvement proposal requires a human (`soma proposals apply`). That
  apply/dismiss step is soma's approval path in the Section 17.1 sense: the
  action does not proceed until a human resolves it.
- **`observe` → near `evaluate_only`, but stricter.** The intent matches -   look, don't act - but the mechanism differs: ACS `evaluate_only` computes
  a verdict and lets the host proceed with the original action; soma's
  `observe` *refuses* execution outright and journals the refusal
  (`autonomy:observe blocks skill.run`). soma has no true shadow mode in
  which a would-be deny is computed but the action proceeds. We state this
  rather than paper over it.

## Fail-closed analysis

ACS requires failing closed to `deny` on every error path (Sections 1.1,
21). soma's gates are deny-by-default by construction:

- An **empty allow list denies everything** (`check_command`: no match →
  deny; covered by a unit test in `src/policy.rs`).
- **Deny globs win** over allow globs, in that order, always.
- The **MCP launcher allowlist** has no fallback to the general command
  list; `/bin/sh`, `bash`, `python3`, `node` are denied even when
  `allow_commands` is `["*"]` (tested).
- **Network** is off by default; the host allowlist ships as
  `127.0.0.1`/`localhost` only.
- **Export and anchor refuse** to operate on a journal that does not
  verify.

One former divergence, now fixed (v0.2.0): ACS requires an
invalid manifest to fail closed with `runtime_error:manifest_invalid`
(Section 2). soma's `Policy::load` used to fall back to the **default
policy** when `.soma/policy.json` was unparseable - the default is
permissive on commands (`allow_commands: ["*"]` minus deny globs), so a
corrupted policy file silently *widened* the command gate. As of
v0.2.0, a policy file that is present but unreadable, unparseable, or
not a JSON object is a hard error that refuses operation, naming the file
and the parse problem; only a *missing* file (fresh project) yields
defaults. Residual leniency, stated plainly: `from_json` still falls back
per-field (e.g. `assist` on an unrecognized autonomy value) once the file
parses as an object.

**Statefulness, scoped carefully.** ACS demands the decision runtime be
stateless: it "MUST NOT retain mutable state that influences a verdict from
one evaluation to the next" (Section 1.1). soma as a whole is deliberately
stateful - reliability scores and lessons accumulate. The distinction that
matters: that state influences skill **selection** (which action to
propose), never a **gate verdict**. Every `check_*` function in
`src/policy.rs` is a pure function of the loaded policy and its input - same policy, same input, same decision, no history consulted. If one reads
ACS as covering selection as a policy decision, soma would *not* conform;
we therefore scope the mapping claim to the gating layer only, and present
selection as an explained, journaled host concern outside the ACS boundary.

## Audit alignment (ACS Sections 19, 13.1, 8)

soma's journal (`src/events.rs`) is an append-only, hash-chained JSONL log:
each event embeds `prev` (SHA-256 of the previous event) and `hash`
(SHA-256 of itself), so `soma log verify` points at the first tampered
line. Redaction by key glob runs **before** anything touches disk. In v6
the chain head is additionally anchored to third-party RFC 3161 timestamp
authorities (`soma anchor now`), which closes the
"operator regenerates and re-signs the chain" attack **for history up to
each anchored seq** - something ACS's audit model does not itself provide.
The honest residue: events appended *after* the last anchor, and truncation
within that trailing window, are not yet covered until the next anchor; and
soma still writes its own journal, so anchoring proves *non-backdating*, not
*completeness*. Event-kind correspondence:

| soma event kind | ACS Section 19 event kind | Notes |
|---|---|---|
| `policy.decision` | `decision` | carries subject, allowed, and the rule that fired |
| `select.explain`, `model.route` | `policy_evaluation` | analogue: the full scored reasoning behind a choice |
| `export.bundle` | shutdown-time audit record | manifest of SHA-256 digests over every file + journal head |
| `wrap.start` / `wrap.end` (v6) | `decision` + timing | spawn receipt is written even if soma is killed mid-run |
| - | `annotator_dispatch`, `evaluation_timing`, `intervention_point.transformed`, `annotator_failed`, `policy_failed` | no analogue; soma has no annotators or transforms |

Where soma does **not** meet Section 19, precisely:

1. **No canonical serialization.** Section 8 requires sorted object
   members at every level as the basis for any hash or audit record.
   soma's hand-rolled JSON preserves insertion order, and event hashes are
   computed over the event as written, not over a sorted-key canonical
   form.
2. **No per-decision action identities.** Section 13.1's
   `input_identity` / `enforced_identity` (`sha256:` + lowercase hex over
   the canonical policy input) are not emitted. soma hashes *events*, not
   *policy inputs*; the two are related but not the same thing.
3. **Raw values in audit.** Section 19 says the runtime "MUST NOT emit
   policy target values, tool arguments or results, model messages,
   secrets, or personal data." soma's journal deliberately records the
   gated command line and bounded output excerpts - it is a local-first,
   operator-owned audit trail, and greppability is the point. Secrets are
   redacted by key pattern before write, but key-glob redaction is not the
   blanket value ban ACS imposes on telemetry. An ACS-grade export would
   emit digests where soma today emits values.
4. **No verdict normalization layer.** soma has no `warn`, `transform`, or
   `escalate` verdict objects, and its deny reasons are free-form rules,
   not drawn from the Section 16 reserved set.

## Gaps (consolidated)

1. ~~Policy-file parse failure falls back to defaults instead of failing
   closed~~ - **fixed in v0.2.0**: an unparseable/invalid
   `.soma/policy.json` now refuses operation outright (see above).
2. Canonical sorted-key serialization (Section 8) not implemented.
3. Per-decision `input_identity`/`enforced_identity` (Section 13.1) not
   emitted.
4. Audit records contain raw command lines and excerpts (Section 19
   redaction rule not met as written).
5. Two-verdict vocabulary; no `warn`/`transform`/`escalate` objects, no
   `evaluate_only` shadow mode (observe refuses rather than shadows).
6. No ACS manifest: `.soma/policy.json` is soma's own schema, not a
   Section 2 manifest; no dispatcher boundary, since soma is host and
   runtime in one binary.

## Subset declaration

Of the eight intervention points (Section 4), soma gates at **three**:

- `agent_startup` - MCP server add/spawn two-gate.
- `pre_tool_call` - command globs, wrap spawn gate, network host
  allowlist, path-write boundary.
- `agent_shutdown` - export/anchor finalization refusing broken chains.

soma does **not** gate `input`, `pre_model_call` (decision-event analogue
only), `post_model_call`, `post_tool_call`, or `output`. Enforcement mode:
`enforce` only (see the autonomy mapping for why `observe` is not
`evaluate_only`). ACS gates conformance on failing closed at unknown
points, not on implementing all eight; the subset is declared so nobody has
to guess.

**Section 20, as actually written:** "An implementation conforms to this
document as a runtime, as a host, or as both." Two roles, no third profile.
A conformant runtime must follow the Section 6 evaluation order, the
Section 7 five-member policy input, Section 8 canonical serialization,
Sections 13–16, and be stateless, deterministic, and fail-closed. A
conformant host must never carry out a denied action, must block on
unresolved escalations, and must substitute transformed policy targets.
soma's gating layer is closest to a *combined* host-and-runtime; this
document is the mapping such a claim requires, not the claim itself.

## ASSERT: evaluation feasibility

[ASSERT](https://github.com/responsibleai/ASSERT) (v0.1.0, MIT, Python
3.11–3.13) is a behavioural evaluation harness - plain-English must-do /
must-never-do specs, generated single- and multi-turn test sets, and an
LLM judge - shipped alongside ACS at Build 2026. It lives under the
`responsibleai` GitHub org, not `microsoft/*`; it is linked as official
from Microsoft's Foundry blog, and we have not independently verified the
org's legal ownership, so we phrase it exactly that way.

How soma would be evaluated:

- **Target shim**: ASSERT accepts a Python callable; a ~20-line adapter
  subprocesses the soma CLI per turn and returns the final text. There is
  no native CLI target type, so the shim is required.
- **Config**: an `eval_config.yaml` whose `behavior` block encodes soma's
  policy contract as must-never-do statements - *never executes a command
  outside the allow globs*, *never contacts a non-allowlisted host*,
  *refuses execution in observe mode* - which is precisely ASSERT's policy
  format.
- **Prerequisite**: ASSERT's generation and judging stages need an LLM API;
  it is not judge-free. The JSON/JSONL artefacts in `artifacts/results/`
  could then be ingested into soma's signed evidence bundle as third-party
  eval evidence.

**Status: planned, not done.** No ASSERT run of soma exists yet; this
section documents feasibility, not results.

## Roadmap

1. **Port the conformance cases.** The reference suite lives at
   `policy-engine/tests/conformance/cases/` in the AGT repo (JSON case
   files keyed to spec sections, with `cases.schema.json`) - note: *not*
   `examples/conformance_snapshots`, which does not exist. Port the cases
   covering points soma implements into `cargo test` and publish pass/fail
   per case.
2. **Emit an ACS manifest.** A soma-generated YAML manifest
   (`agent_control_specification_version: 0.3.1-beta`) declaring soma's
   glob engine as a Section 12.1 `custom` policy adapter - spec-legal
   without taking a Rego or Cedar dependency.
3. **Per-decision action identities.** Canonical sorted-key serialization
   of a five-member policy input and `sha256:` `input_identity` /
   `enforced_identity` on every `policy.decision` event (Gaps 2–3).
4. **Fail closed on a bad policy file.** Done (Gap 1, v0.2.0):
   the silent default-policy fallback is replaced with a hard refusal
   (no journaling - the runtime never proceeds far enough to act).
5. **ASSERT run** per the section above, results into the evidence bundle.

---

ACS `0.3.1-beta` is a Draft; its own Section 22 reserves the right to break
the contract between minor versions while the pre-release tag is present.
This mapping is self-asserted, pinned to the spec text fetched 2026-06-12,
and will be revised when the spec moves. Corrections welcome - every claim
above names the file that proves or disproves it.

*soma project - 2026-06-12.*
