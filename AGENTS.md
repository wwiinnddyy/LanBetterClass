# AGENTS.md

Guidance for AI coding agents working in this repository.

## Build & test policy: CI-only (GitHub Actions)

**Do all compilation, testing, and packaging through GitHub Actions CI/CD — never locally.**
This machine's disk is a hard constraint; local `target/` build output is what we are trying to avoid.
See the `.qoder/skills/ci-only-build/` skill for the full workflow.

### Never run locally

`cargo build`, `cargo check`, `cargo test`, `cargo run`, `cargo clippy`, or any package-manager build —
all of these write to `target/` and are forbidden by default.

### Allowed locally

- Reading and editing source, JSON manifests, and docs.
- `cargo fmt --check` (formatting only — produces no build artifacts).
- `git` operations (commit, push).
- Inspecting CI runs / downloading artifacts with `gh`.

Only build locally if the user explicitly asks.

### How to verify a change

```bash
git push origin "$(git rev-parse --abbrev-ref HEAD)"
gh workflow run ci.yml --ref "$(git rev-parse --abbrev-ref HEAD)"
gh run watch                 # follow the latest run
gh run view --log-failed     # read failures
gh run download <RUN_ID> --name bins-windows-latest --dir .ci-artifacts
```

Treat a green `ci` run as the proof that code builds and works. Report results by citing the CI run,
not a local build. `.ci-artifacts/` is scratch — keep it gitignored; do not overwrite the demo binaries
in `try/` unless asked to refresh them.

## Project at a glance

ClassAgent is a classroom **data-collection core**. It only understands "events", never a concrete data
source: whiteboard ink, audio, ASR, screen keyframes, courseware, and evaluation are normalized into an
NDJSON event stream over stdio, appended to disk, then folded into an AI-readable timeline. Platform
differences (DXGI / PipeWire / WASAPI) live in out-of-process **adapters**, so adding a school environment
never requires recompiling the core.

- `crates/schema` — the single contract shared by core and adapters (types + `kind` conventions).
- `crates/core` — the core CLI + local dashboard (`protocol`, `supervisor`, `store`, `timeline`, `digest`, `serve`).
- `adapters/*` — out-of-process data sources (e.g. `a-fake`, `a-audiofile`).
- `adapters.d/*.adapter.json` — per-environment load declarations (drop in = enable, rename = disable).

### Notes that affect CI

- `tools/ci-smoke.sh` runs identically on Linux and Windows runners and asserts real behavior; it is the
  end-to-end guard the CI relies on. Keep assertions running on CI, not locally.
- `debug = 0` and `strip`/`lto` are set in the workspace `Cargo.toml` because disk size matters; do not
  reintroduce heavy debug artifacts (e.g. Windows PDBs) casually.
