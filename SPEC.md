# SOMA - Specification v0.1

**soma** /ˈsoʊmə/ - the cell body of a neuron: the part that integrates incoming
signals, maintains the cell, and decides what fires.

A transparent, policy-governed, self-improving agent runtime. Zero-dependency Rust.

## Positioning

The HYT ecosystem already has verification and memory:

| Tool | Role |
|---|---|
| **agef** | Open spec for portable, tamper-evident agent session evidence |
| **Akmon** | Evidence capture, signing, offline verification (AGEF reference impl) |
| **memora** | Verified memory - claims with cryptographic citation integrity |
| **soma** (this) | The missing piece: the *execution* runtime - skills, selection, goals, autonomy |

soma executes work through small purposeful programs (skills), explains every
choice it makes, journals everything in a tamper-evident log, and improves its
own toolbox over time - under policies the operator defines.

Design contrast with the alternatives (and deliberately with Akmon's own stack):
**zero external crates**. No tokio, no serde. Hand-rolled JSON, SHA-256, cron
parser, and HTTP. Rationale: minimal supply-chain surface (auditable by reading
`src/`), fast cold builds, small binary, low and predictable RAM - soma should
run on machines that can barely run a local model.

## Requirements

Each requirement below is traceable; the verification workflow checks each one
against the implementation and the report records any deviation.

### Transparency & audit

- **R1 - Hash-chained journal.** Every significant action (skill run, model
  call, policy decision, selection, proposal, cron tick, goal step) is an event
  appended to a per-project JSONL journal. Each event carries `prev` = SHA-256
  of the previous event line, forming a tamper-evident chain.
  `soma log tail|show|verify` - verify recomputes the chain and reports the
  first broken link, exit non-zero on tamper.
- **R2 - Exportable history.** `soma export` produces a portable bundle
  directory (and `.tar.gz`): `manifest.json` (producer, project, event count,
  head hash, time range, sha256 of every file) + `events.jsonl` + state
  snapshots (skills, metrics, goals, proposals, knowledge). Verifiable offline
  with `sha256sum` + any JSON reader. AGEF-inspired; full AGEF conformance via
  Akmon import is a documented path, not reimplemented here.
- **R3 - Policy engine.** `policy.json` per project: autonomy level
  (`observe|assist|auto` - the per-action approval model), command allow/deny
  patterns, network permission + host allowlist, writable-path boundaries
  (enforced on paths soma itself writes, e.g. `export --out`; shell execution
  is bounded by the command allow/deny list, not a path sandbox - documented
  in README), and secret-redaction patterns applied before journaling. Every
  allow/deny decision - command, execution, **network**, and path - is itself
  a journaled event naming the rule that fired.

### Skills - small programs with purpose

- **R4 - Skill manifests.** A skill is a manifest `skill.json`:
  `{name, version, purpose, goal, tags[], kind: command|mcp, run{cmd,timeout_s},
  success{exit0|contains}, notes}`. Registries: global (`$SOMA_HOME/skills/`)
  and per-project (`.soma/skills/`). CLI: `soma skill list|show|add|run|lint`.
- **R5 - Skill health.** Per-skill metrics persisted: runs, successes,
  failures, avg duration, last used. A failed run automatically files an
  **issue** against the skill (with stderr excerpt). `soma skill issues`.
- **R6 - Neuro selector.** `soma select "<task>"` ranks all known skills with a
  deterministic, explainable score: tag match + token overlap (purpose/goal) +
  Laplace-smoothed success rate + recency + knowledge-lesson boost. Output: the
  chosen skill, per-factor score breakdown for the top candidates, and a plain
  English "chosen because / runner-up because" explanation. `--run` executes
  the winner through the policy gate. Optional `--ask-model` re-rank.

### Self-improvement

- **R7 - Improvement proposals.** An engine scans metrics, issues, and the
  journal and emits proposals `{kind: fix_skill|tune_timeout|archive_skill|
  add_cron|config_change|new_skill, target, rationale-with-numbers,
  suggested_change}`. `soma proposals list|show|apply|dismiss`. `apply` performs
  the mechanical change (e.g. bump timeout, write cron entry), bumps the skill
  version where relevant, and journals it. Policy `auto` may auto-apply
  mechanical proposals at `soma tick`.
- **R8 - Knowledge base.** Per-project `knowledge.jsonl` of entries
  `{kind: lesson|note|reference, title, body, tags}`. Lessons are auto-recorded
  when an issue is resolved by an applied proposal. `soma knowledge
  add|list|search` (token-overlap ranking). The selector consumes matching
  lessons as a scoring boost (R6). Backend is a trait so memora can later be an
  alternative store.

### Models - hybrid, local-first

- **R9 - Providers.** `ModelProvider` trait with three implementations:
  `echo` (testing, offline), `ollama` (local, plain HTTP over TcpStream -   no TLS dependency), `anthropic` (cloud, via `curl` subprocess so TLS comes
  from the OS; key from `ANTHROPIC_API_KEY`; policy-gated network). `soma model
  list|probe|ask`.
- **R10 - Hybrid router.** A deterministic difficulty heuristic classifies a
  task `simple|moderate|complex` with named factors (length, code markers,
  multi-step markers, design vocabulary). Routing table in config maps
  difficulty→tier (e.g. simple→local-small, complex→cloud-big). Unreachable
  provider ⇒ logged fallback to next tier. `soma model route "<task>"` explains
  the classification and the chosen provider.
- **R11 - Response cache.** Content-addressed: key = SHA-256(provider+model+
  prompt+params). LRU with configurable byte cap. Hits journaled; `soma cache
  stats|clear` reports hit rate and estimated saved calls.

### Direction & automation

- **R12 - Goals & workflows.** Goal = `{title, why, acceptance[]}` plus ordered
  steps `{name, kind: skill|model|command, input, verify: exit0|contains|command}`.
  `soma goal add|step|run|status|show`. Run executes steps through policy,
  verifies each, halts or continues per flag, journals everything, and reports
  acceptance status at the end.
- **R13 - Crons.** `crons.json`: `{schedule: 5-field cron (UTC: * lists ranges
  steps), action: skill|goal, enabled}`. `soma cron add|list|due|tick`; `tick`
  runs due jobs through policy and records last/next run.
- **R14 - Cron proposer.** ≥3 manual runs of the same skill/goal at a roughly
  regular cadence ⇒ an `add_cron` proposal with the inferred schedule (R7
  pipeline).
- **R15 - Optimizer.** `soma optimize` analyzes journal, metrics, and cache:
  proposes config improvements (cache size, routing mix - e.g. "80% of cloud
  calls were classified simple → route simple to local", slow/flaky skills,
  low-RAM preset) through the proposal pipeline.

### Integration & operation

- **R16 - MCP client.** `mcp.json` configures stdio servers `{command,args,env}`.
  Newline-delimited JSON-RPC 2.0: `initialize` → `tools/list` → `tools/call`.
  `soma mcp tools|call|import` - import materializes server tools as `kind:mcp`
  skills so the selector can choose and explain them. Frames are journaled.
- **R17 - Projects & presets.** `soma init` creates `.soma/` in a directory and
  registers it in `$SOMA_HOME` (default `~/.soma`, overridable). `soma project
  list`. Builtin presets `local-only|hybrid|cloud-max|low-ram` applied with
  `soma preset apply <name>` (writes config+policy, journaled).
- **R18 - Lean & low-RAM.** Zero external crates; no async runtime; streaming
  line-by-line I/O for journals; bounded in-memory structures; release binary
  in the low single-digit MB; rationale and measured footprint documented in
  README.

## Acceptance

1. `cargo build --release` and `cargo test` pass clean.
2. Every R has working CLI surface demonstrated in the dogfood run
   (`soma` builds itself a project, runs a goal whose steps build/test soma,
   and exports the evidence bundle).
3. The verification workflow signs off each R as full/partial/missing with
   evidence; deviations land in REPORT.md.

## Out of scope (v0.1)

Long-running daemon (cron is tick-based; launchd/systemd snippet documented),
Ed25519 signing (delegated to Akmon), embeddings/vector search (memora's job),
TUI, Windows.
