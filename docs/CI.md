# soma in CI - governed runs with portable evidence (v6/D12)

`ci/github-action/` ships `soma-governed-run`, a composite GitHub Action that
runs one command under soma governance and leaves behind evidence a stranger
can verify with `shasum`, `openssl`, and `jq` - no soma, no accounts, no
vendor trust.

> **Status - honest note:** validated locally (every step of the action was
> executed by hand end to end on 2026-06-12; transcript in the v6 close-out),
> **CI-untested** - this repository is not public on GitHub yet, so the
> action has never run on a real runner. Treat the workflow below as
> reviewed-but-unproven until the repo is public.

## What one governed run produces

In `out-dir` (default `soma-evidence/`):

| artifact | what it is |
|---|---|
| `<project>-attestation-<stamp>.json` | in-toto Statement v1. Subject digest = the journal head - a real SHA-256 over the hash-chained events. **Unsigned by soma** (zero-dep stance); sign it in the workflow (step 5 below). |
| `ci-<stamp>.soma-export/` | the evidence bundle: full journal, policy/config snapshots, anchors (when any), `manifest.json` with per-file SHA-256s, and `VERIFY.md` with the no-soma verification walkthrough. |
| `ci-<stamp>.soma-export.tar.gz` | the same bundle, one file. |

The attestation is exported **before** the bundle, so the bundled journal
contains the attestation's own `export.bundle` receipt - that receipt's
`data.head` equals the attestation's subject digest, which is how a reviewer
ties the attestation to the chain they verified (see below).

## Caller workflow (copy-paste)

```yaml
name: governed-agent-task
on: [push]

permissions:
  contents: read
  # only needed for the optional signing step (5):
  id-token: write

jobs:
  governed:
    runs-on: ubuntu-latest
    steps:
      # 1. Your repo - the thing the governed command works on.
      - uses: actions/checkout@v4

      # 2. soma, PINNED to a commit you reviewed: this code builds and runs
      #    inside your workflow. Never pin a branch.
      - uses: actions/checkout@v4
        with:
          repository: radotsvetkov/soma   # adjust when published
          ref: <reviewed-commit-sha>
          path: soma

      # 3. The governed run. ~5 s cold build (zero deps; runners ship rust).
      - id: soma
        uses: ./soma/ci/github-action
        with:
          run: "npm test"            # the command to govern
          label: "ci"
          preset: "local-only"       # hybrid-default/cloud-max to allow TSA hosts
          # anchor: "true"           # needs a TSA-allowing preset + network
          out-dir: "soma-evidence"

      # 4. Upload the evidence - the action deliberately does not do this
      #    (composite actions stay pure bash; third-party actions are the
      #    caller's choice). if: always() - evidence matters MOST on failure,
      #    and the action exits with the wrapped command's code.
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: soma-evidence
          path: soma-evidence/

      # 5. OPTIONAL - sign the attestation. soma emits it unsigned by
      #    design; the workflow's OIDC identity signs it into Sigstore
      #    (keyless cosign shown; `gh attestation` / actions/attest are the
      #    GitHub-native equivalent).
      - if: always()
        run: |
          cosign sign-blob --yes \
            --bundle "${{ steps.soma.outputs.attestation-path }}.sigstore.json" \
            "${{ steps.soma.outputs.attestation-path }}"
```

Action outputs: `journal-head` (the attested head), `bundle-path`,
`attestation-path`.

Failure semantics: if the wrapped command exits non-zero, the action exports
the evidence **and then** exits with that code - the job fails, the evidence
survives. `anchor: "true"` that gets refused (no network, or a preset without
the TSA host - `local-only` never has it) also fails the job, after export.

## What the reviewer does with the PR artifact

Download and unpack (`gh run download -n soma-evidence`, or the web UI), then
verify on any machine - soma not required.

### 1. Chain - soma-free, shasum only

Open `VERIFY.md` inside the `.soma-export` bundle; it is the authoritative
walkthrough. The essence:

```sh
cd ci-<stamp>.soma-export
# every file matches the manifest:
shasum -a 256 events.jsonl    # compare to manifest.json → files → sha256
# the chain: per line of events.jsonl, `hash` = SHA-256 of the line with the
# `,"hash":"…"` member removed, and `prev` = the previous line's `hash`
# (first line: "genesis"). manifest.json:journal_head = the last line's hash.
```

Any edited, removed, or reordered event breaks every later link.

### 2. Anchors - openssl, third-party clock (when present)

The bundle's `VERIFY.md` ANCHORS section carries the exact commands,
including where to fetch the TSA root CA. The shape:

```sh
openssl ts -reply -in anchors/<file>.tsr -text       # inspect genTime
openssl ts -verify -queryfile anchors/<file>.tsq \
    -in anchors/<file>.tsr -CAfile <root.pem>        # expect: Verification: OK
```

A wrong digest MUST fail with `message imprint mismatch` - try it as a
negative control.

### 3. Attestation - jq

```sh
A=<project>-attestation-<stamp>.json
jq -r '._type, .predicateType' "$A"
#  https://in-toto.io/Statement/v1
#  https://github.com/radotsvetkov/soma/evidence/v1
jq '.subject[0], .predicate.chain, .predicate.policy, .predicate.kinds' "$A"

# Tie it to the chain you verified in step 1: the attested head must appear
# in the bundled journal as the head recorded by the attestation's own
# export receipt (and as the `hash` of the line before it):
DIGEST=$(jq -r '.subject[0].digest.sha256' "$A")
grep '"format":"attestation"' ci-<stamp>.soma-export/events.jsonl \
  | grep -c "$DIGEST"     # expect: 1
```

If the attestation was signed (step 5), additionally verify the signature
(`cosign verify-blob --bundle … --certificate-identity-regexp …` against the
workflow's identity, or `gh attestation verify`).

### What this does and does not prove

The chain proves internal consistency of what was journaled; the anchor pins
the head to a third party's clock; the attestation binds head + policy +
event histogram into one signable subject. None of it sandboxes the wrapped
command or proves the absence of unrecorded activity - soma's honest-limits
stance (see `soma help`, wrap section) applies in CI unchanged.
