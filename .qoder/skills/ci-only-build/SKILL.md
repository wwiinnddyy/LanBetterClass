---
name: ci-only-build
description: Build, test, and package this project exclusively through GitHub Actions CI/CD instead of local cargo/npm builds, to conserve the user's local disk. Use whenever code must be compiled, built, tested, run through the smoke suite, packaged, or when binaries/artifacts are needed — and whenever the user mentions build, compile, test, run, cargo, npm, package, artifacts, or "does it work / is it verified". Never reports something as built or tested from a local toolchain.
---

# CI-Only Build

## Core rule

All compilation, testing, and packaging happen on **GitHub Actions**, never on the local machine.
Local `target/` / `node_modules/` build output is the single biggest disk cost on this box, so it is
not produced locally at all. The agent edits code, pushes, lets CI build + test + smoke, then
downloads artifacts if a binary is needed.

This is a **strict** policy: no local `cargo build` / `cargo check` / `cargo test` / `cargo clippy` /
`cargo run` — all of these write to `target/` and defeat the purpose.

## Allowed locally vs. forbidden

| Do locally (no compilation) | Never locally (produces `target/`) |
| --- | --- |
| Read / edit source, JSON, docs | `cargo build` |
| `cargo fmt --check` (formatting only) | `cargo check` |
| `git` operations (add/commit/push) | `cargo test` / `cargo run` |
| Inspect CI logs / artifacts via `gh` | `cargo clippy` |
| Reason about the code | `npm build` / any package-manager build |

If a check genuinely can't wait for CI, ask the user; default to CI.

## The workflow

Track these steps for any "make it build/verify" task:

```
- [ ] 1. Edit code locally (no build).
- [ ] 2. cargo fmt --check + git commit + push a branch.
- [ ] 3. Trigger / observe the ci workflow on that branch.
- [ ] 4. Watch the run; read failed logs.
- [ ] 5. On success: download artifacts if a binary is needed. On failure: fix, push, re-run.
- [ ] 6. Report results citing the CI run URL, not a local build.
```

### Concrete commands

Environment already verified: `gh` CLI installed and authed (scopes `repo`, `workflow`), remote is a
GitHub repo, workflow file `.github/workflows/ci.yml` (name `ci`) with `workflow_dispatch` + build
matrix `windows-latest` / `ubuntu-latest`.

```bash
# 2. push the working branch
git push origin "$(git rev-parse --abbrev-ref HEAD)"

# 3. run the pipeline on the current branch (workflow_dispatch exists in ci.yml)
gh workflow run ci.yml --ref "$(git rev-parse --abbrev-ref HEAD)"

# 3b. or just let a push / PR trigger it, then find the run:
gh run list --workflow ci.yml --limit 5

# 4. watch it and read failures
gh run watch                       # follows the latest run
gh run view --log-failed           # only the failing steps' logs
gh run view <RUN_ID>               # full job/step detail

# 5. download built binaries (names follow bins-<os> from ci.yml)
gh run download <RUN_ID> --name bins-windows-latest --dir .ci-artifacts
gh run download <RUN_ID> --name bins-ubuntu-latest  --dir .ci-artifacts
```

Download target `.ci-artifacts/` is scratch — add it to `.gitignore` if it isn't already, so artifacts
never dirty the working tree. The committed demo binaries live in `try/`; do not overwrite them from CI
unless the user asks to refresh the demo.

## What CI runs (so you trust it as the verifier)

`.github/workflows/ci.yml` per OS does: `cargo build --workspace --all-targets` → `cargo test
--workspace` → `bash tools/ci-smoke.sh` (end-to-end: event delivery, seq gap detection, crash-restart
seq space, blob contract, budget enforcement, timeline alignment, dashboard path-traversal guards) →
uploads `bins-<os>`. Treat a green `ci` run as the proof of "it builds and it works". The smoke script
itself states the reason it runs only on CI: local disk does not allow local builds.

## Failure loop

1. `gh run view <RUN_ID> --log-failed` — read the exact failing step.
2. Fix source locally.
3. `git commit` + `git push` + `gh workflow run ci.yml --ref <branch>` again.
4. Do not claim success until a run is green.

## Reporting

When answering "did it build / is it tested / does it work", cite the CI run (URL from
`gh run view <RUN_ID>`), the OS matrix that passed, and any artifact path you downloaded. Never imply a
local compile happened.
