# soma JSON CLI contract - ui_api 1

This file is the single source of truth for the
`--json` surface; the cockpit is built against it. Field names mirror the
journal/state vocabulary - no new naming layer.

## Rules

1. `--json` changes **stdout only**. Errors remain human text on **stderr**
   with non-zero exit. No JSON error envelope.
2. Streams are NDJSON (one JSON document per line); everything else emits
   exactly one JSON document.
3. Where the data already exists as stored JSON (journal events, proposals,
   issues, knowledge entries, skill manifests), it is passed through
   **unchanged** (serialized with the existing `json` module) - never
   re-shaped.
4. Additive evolution only: new fields may appear; existing fields never
   change meaning within the same `ui_api`.
5. `select --json` is incompatible with `--run` and `--ask-model` → error.
   (the cockpit select surface is dry by design)
6. Version bump: `SOMA_VERSION` → `0.2.0`; new `pub const UI_API: u32 = 1`.

## Commands

| Command | Output |
|---|---|
| `version --json` | `{"version":"0.2.0","ui_api":1}` |
| `status --json` | `{"project":s,"root":s,"autonomy":s,"network":{"allow":b,"hosts":[s]},"events":n,"skills":n,"open_issues":n,"open_proposals":n,"goals":n,"crons":n}` (reuse what `cmd_status` already computes; counts may be best-effort but keys above are required) |
| `log tail [-n N] --json` | NDJSON: the last N raw journal lines **verbatim** (they are already JSON events) |
| `log show <id> --json` | the raw event object |
| `log verify --json` | ok: `{"ok":true,"events":n,"head":s}` · broken: `{"ok":false,"events_checked":n,"broken_line":n,"reason":s}` + exit 1 |
| `skill list --json` | `[{"name":s,"version":n,"kind":s,"purpose":s,"goal":s,"tags":[s],"archived":b,"origin":"project"\|"global","runs":n,"successes":n,"failures":n,"last_used_ms":n\|null,"open_issues":n}]` |
| `skill show <name> --json` | `{"manifest":<manifest as stored>,"origin":s,"runs":n,"successes":n,"failures":n,"last_used_ms":n\|null,"open_issues":n}` |
| `issues list [--all] --json` | array of issue objects as stored (latest state per id; default open only, `--all` includes resolved) |
| `select "<task>" --json` | `{"task":s,"chosen":<cand>\|null,"candidates":[<cand>… max 5]}` where `<cand>` = `{"name":s,"score":x,"origin":s,"kind":s,"factors":[{"name":s,"value":x,"note":s}]}` - mirror the existing `Selection`/`Candidate`/`Factor` structs; still journals `select.explain` |
| `proposals list [--all] --json` | array of proposal objects as stored (default open only) |
| `proposals show <id> --json` | proposal object as stored |
| `proposals apply <id> --json` | `{"ok":b,"id":s,"action":"applied","note":s}` (note = the human summary) |
| `proposals dismiss <id> --json` | `{"ok":b,"id":s,"action":"dismissed","note":s}` |
| `model probe --json` | `[{"provider":s,"ok":b,"note":s}]` |
| `model route "<task>" --json` | `{"task":s,"difficulty":"simple"\|"moderate"\|"complex","points":n,"factors":[{"points":n,"note":s}],"provider":s,"model":s}` |
| `cache stats --json` | `{"entries":n,"bytes":n,"max_bytes":n,"hits_total":n}` |
| `project list --json` | `[{"name":s,"root":s}]` |
| `knowledge list --json` | array of entries as stored |
| `knowledge search "<q>" --json` | `[{"score":x,"entry":<entry as stored>}]` |
| `goal list --json` | array of goal objects as stored (latest record per id wins; steps included) |
| `goal show <id> --json` | the latest stored goal object for that id |
| `cron list --json` | array of stored cron entries each augmented with `"next_iso"`: next run as ISO-8601 UTC string, or `null` if disabled or no future match within 366 days |
| `config get [path] --json` | with no path: the whole config object; with `dotted.path`: the leaf value as JSON (string, number, bool, object - whatever is stored) |
| `policy show --json` | the full policy object: `{"autonomy":s,"allow_commands":[s],"deny_commands":[s],"allow_network":b,"allow_hosts":[s],"writable_paths":[s],"redact_keys":[s],"max_timeout_s":n}` |
| `wrap [flags] --json -- <cmd> [args...]` | the wrapped child's stdout/stderr stream through live (tee) first; on completion exactly one JSON object - the `wrap.end` event data as journaled: `{"label":s,"pid":n,"exit":n,"duration_ms":n,"stdout_sha256":s,"stderr_sha256":s,"stdout_bytes":n,"stderr_bytes":n,"stdout_excerpt":s,"stderr_excerpt":s,"timed_out":b}` (`pid` carried from `wrap.start` so the EU AI Act period-of-use pass pairs starts↔ends on `(label, pid)`). soma's exit code is the child's (`124` after a `--timeout-s` kill). Policy refusals follow rule 1: human text on stderr, non-zero exit, nothing spawned. |
| `anchor now [--url U] --json` | on success exactly one object - the `journal.anchor` event data as journaled: `{"seq":n,"head":s,"url":s,"tsq_file":s,"tsr_file":s,"tsr_sha256":s,"status":"granted"}`. Failures (policy refusal, curl failure, non-granted PKIStatus) journal `journal.anchor` with `status:"failed"` + `reason`, then follow rule 1: human text on stderr, non-zero exit. |
| `anchor list --json` | array of the stored `journal.anchor` events **verbatim** (full event objects; rule 3) |
| `anchor verify [<seq>\|--all] --json` | per anchor: `{"seq":n,"head":s,"ok":b,"checks":{"chain":{"ran":b,"ok":b,"note":s},"tsr_file":{"ran":b,"ok":b,"note":s},"openssl":{"ran":b,"ok":b,"note":s}}}` - `ok` covers the required checks (chain, tsr_file); openssl is best-effort/advisory. No seq → the latest granted anchor (one object); `--all` → array. On a required-check failure the JSON is printed to stdout, then non-zero exit (same pattern as `log verify --json`). |
| `export eu-ai-act [--out FILE.md]` | no `--json` flag - writes `<name>.md` plus a machine-readable `<name>.json` **file** sibling (default `exports/<project>-aiact-<stamp>.md/.json`). Refuses on a broken chain. JSON document keys (stable; additive evolution per rule 4): `{"generator":{"name":"soma","version":s,"format":"eu-ai-act","format_version":n},"generated_at":s,"legal_status_snapshot":s,"regulation":{"celex":s,"eli":s},"application_dates":{"in_force":{"annex_iii":s,"annex_i":s},"omnibus_provisional":{"annex_iii":s,"annex_i":s,"status":s}},"caveats":["C1"…"C8"],"system":{"project":s,"system":s,"provider":s,"deployer":s,"intended_purpose":s,"classification":s},"journal":{"events":n,"head":s,"verified":b,"first_event":s,"last_event":s,"age_days":n},"kinds":[{"kind":s,"count":n,"first":s,"last":s,"art12_2a":b,"art12_2b":b,"art12_2c":b}],"periods_of_use":[{"source":"wrap"\|"skill.run"\|"goal.run","label":s,"start":s,"end":s\|null,"duration_ms":n\|null,"exit":n\|null,"ok":b\|null,"timed_out":b}],"anchors":[{"seq":n,"head":s,"url":s,"tsr_file":s,"status":"granted"}],"retention":{"floor_months":6,"journal_age_days":n,"journal_first_event":s,"soma_deletes_logs":false},"policy":{"autonomy":s,"denials":n}}`. Missing operator fields (`config set aiact.system\|provider\|deployer\|intended_purpose\|classification`) carry the literal placeholder string `[NOT PROVIDED - operator must complete]`, never omitted. `aiact.classification` is validated to `high-risk-6-1\|high-risk-6-2\|not-high-risk-6-3\|out-of-scope`. The generation is journaled as `export.bundle` with `"format":"eu-ai-act"`. |
| `export attestation [--subject N] [--out FILE.json]` | no `--json` flag - writes one in-toto Statement v1 **file** (default `exports/<project>-attestation-<stamp>.json`). Refuses on a broken chain - `predicate.chain.verified:true` is never emitted falsely. Keys (stable; additive evolution per rule 4): `{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":s (project name, or the --subject override),"digest":{"sha256":s}}],"predicateType":"https://github.com/radotsvetkov/soma/evidence/v1","predicate":{"soma_version":s,"project":s,"generated":s,"event_count":n,"first_event":s,"last_event":s,"kinds":{"<kind>":n,…},"policy":{"autonomy":s,"deny_commands_count":n,"allow_commands_count":n,"network":{"enabled":b,"hosts_count":n}},"anchors":[granted journal.anchor data objects **verbatim** (rule 3): {"seq":n,"head":s,"url":s,"tsq_file":s,"tsr_file":s,"tsr_sha256":s,"status":"granted"}],"chain":{"seq":n,"head":s,"verified":true}}}`. The subject digest is honest: the journal head IS a SHA-256 over the hash-chained events, so the digest binds the exact evidence chain - not a checksum of some file soma could regenerate. soma does NOT sign the statement (zero-dep stance); it is the input for `cosign` / `gh attestation` in CI (docs/CI.md, ci/github-action/). The generation is journaled as `export.bundle` with `"format":"attestation"`. |

(`s`=string, `n`=integer, `x`=number, `b`=bool.)

## Acceptance

- Every command above has at least one test that parses stdout with the
  crate's own `json::parse` and asserts the required keys.
- All pre-existing tests stay green. `cargo build --release` clean.
- **Zero new dependencies** (Cargo.toml untouched except version if present).
- `soma help` mentions `--json` where supported, briefly.
