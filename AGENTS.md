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

The observer app is built by its own workflow; its binary is never produced locally either:

```bash
gh run list --workflow agent --limit 1
gh run download <RUN_ID> --name agent-windows-latest --dir .ci-artifacts
```

Treat a green `ci` run as the proof that code builds and works. Report results by citing the CI run,
not a local build. `.ci-artifacts/` is scratch — keep it gitignored; do not overwrite the demo binaries
in `try/` unless asked to refresh them.

## Project at a glance

ClassAgent is a classroom observation system split into **three modules that live in one repository**;
the top-level directory is the module boundary, so a glance at the tree tells you which side you're on.
The core idea is unchanged: the client only understands "events", never a concrete data source —
whiteboard ink, audio, ASR, screen keyframes, courseware, and evaluation are normalized into an NDJSON
event stream over stdio, appended to disk, then folded into an AI-readable timeline. Platform differences
(DXGI / PipeWire / WASAPI) live in out-of-process **adapters**, so adding a school environment never
requires recompiling the client.

| Module | Dir | Binary | Responsibility |
| --- | --- | --- | --- |
| **client** | `client/` | `classagent-client` | Collects events from adapters, stores them, exports `ai_payload`, pushes to the server, serves a local read-only API |
| **server** | `server/` | `classagent-server` | Receives uploads (`POST /api/ingest`), dedups on disk, emits the deterministic `ai_request.json` projection plan |
| **agent** | `agent/` | Tauri desktop app | The observer: connects client + server in one UI for visual configuration and review |
| shared contract | `shared/` | crate `classagent-schema` | Single contract reused by client and server (types + `kind` conventions); name kept stable for imports |

- `client/src/` — `protocol`, `supervisor`, `store`, `timeline`, `digest`, `serve`, `push`.
- `client/adapters/*` — out-of-process data sources: `a-fake` (scripted), `a-audiofile` (blob convention),
  `a-audio` (the real one: cpal capture + energy-VAD turns; `source: "fixture"` replays a wav through the
  same pipeline, which is what makes it testable on a runner with no sound card),
  `a-screen` (keyframes: GDI BitBlt or DXGI desktop duplication + dHash change gate; `source: "fixture"`
  replays a directory of PNGs through the same decide-and-store path — same trick, same reason).
- `adapters.d/*.adapter.json` — per-environment load declarations (drop in = enable, rename = disable).

**Adapter lifecycle contract**: `StopLesson` is where an adapter flushes, and the client keeps reading its
stdout for a short quiet window after sending it and before closing the lesson (`main.rs::drain_tail`: one
400 ms window of silence, 3 s hard cap). Anything emitted after `Stop` lands in `misc.ndjson`, i.e. outside
the lesson — never defer a `session.close` or a final blob to `Stop`. This ordering once silently dropped a
whole lesson's audio summary on real hardware.

Flushing must also be *prompt*, not merely early. A pacing sleep that won't wake until the next poll period
answers `StopLesson` a whole `poll_ms` late; if that exceeds 400 ms the tail lands after the client already
closed the lesson, and because the process is exiting too, the last frame and its `session.close` vanish
with no `misc.ndjson` trace at all. This is what happened to `a-screen`'s replay clock on the first CI run:
the adapter's own stderr reported 11 frames, the lesson had 10. Every wait inside an adapter is therefore
interruptible by the stop signal (`a-screen::wait_until`), and `tools/ci-screen.sh` ④ is the guard that
catches the regression — not because the assertion is clever, but because a dropped tail is a whole missing
sentence.

**What is a lesson fact vs. a capture-process fact**: `session.close` and `core.respawn` are facts about the
capture process, not things that happened in the classroom, so `timeline::build` keeps them out of `track`
and folds them into `sources[<id>].close` / `.restarts` instead. Putting them in `track` would also inflate
`stats.duration_ms` (which is `max(t1_ms)`) into wall-clock process time. Per-segment numbers a teacher or
model can actually read are `track[].detail` (`rms` / `peak` / `speech_ms` for audio, `trigger` / `dist` /
`mad` / `dirty` for keyframes — only for sources that reported them, absent is not zero).

`CloseStats` is a closed struct, but adapters keep inventing new close facts, so `timeline` harvests every
key it does not recognize into `sources[<id>].close_extra` (a `Value`). That is deliberate: adding a
diagnostic to an adapter must not require a core change, and the observer/digest read whatever was reported.

### Keyframe semantics (`a-screen`) — four things that are not obvious from the code

- **Two gates, joined by OR**: `min_dist` (dHash Hamming distance, max 64) catches layout change but is blind
  to a region inverting black→white (neighbour relations survive); `min_mad` (mean absolute grid difference)
  catches that, and is in turn blind to a same-cell content swap. Raising only one therefore often fails to
  reduce frames — the UI has to say so, and does.
- **A rejected sample must not update the reference frame.** If it did, a slow page scroll would read as
  "changed a little each time" and a whole board page would vanish silently. The reference is the last
  *emitted* frame; `unchanged` / `throttled` / `capped` counts land in `close_extra`.
- **`capture` has exactly one source of truth**: `capture.rs::BACKENDS`. `serve.rs`'s 400-whitelist and the
  observer's dropdown are cross-asserted against it (`tools/ci-screen.sh` ⑧ + `agent.yml`), because drift
  there is invisible at runtime — the value writes fine and the adapter just never starts.
- **`stats.stream_errors` now has two owners** (audio dropouts, DXGI `ACCESS_LOST` rebuilds). Any wording
  that says "录音" is wrong; `timeline` and `digest` say "采集流错误" and name the source.

### Configuration semantics (undocumented, this bites)

`classagent-client serve --allow-write` accepts `POST /api/adapter/<file>` carrying **only** `enabled` and
`params`; `argv` / `cwd` / `id` / `platforms` are refused with 400. `--allow-write` rests on "the teacher on
this machine is trusted", and a writable `argv` would turn a configuration endpoint into local code
execution. `params` is deep-merged, so submitting one `vad.rms_open` never deletes its sibling keys.

The endpoint also refuses values the adapter would silently clamp: `screen.min_dist` above 64 (dHash is 64
bits) or `poll_ms: 0` (clamped to 1ms = one grab per millisecond) are 400s, not warnings. A threshold that
gets clamped produces a lesson with almost no frames and no clue as to why.

Edits take effect **next lesson**, and that is structural rather than lazy: `serve` and `run` are separate
processes with no IPC between them, so the write endpoint can only touch the file. `start_lesson` re-reads
each declaration and re-sends `Configure` (`supervisor::reload_params`) before dispatching `StartLesson`;
a crash-respawn re-reads too (`refresh_params`), so a source that dies never silently reverts to stale
thresholds. Making a change land mid-lesson would mean kill + respawn of that one source, which splits the
sentence being spoken and opens a second seq generation — deliberately out of scope. The observer UI shows
"declared on disk" beside "actually in effect this lesson" precisely because those two can differ.

`agent/` is deliberately **not** a member of the root workspace: it carries its own `[workspace]` so the
heavyweight webview dependency tree never enters the client/server pipeline. It builds in its own
workflow, `.github/workflows/agent.yml`.

### Notes that affect CI

- `tools/ci-smoke.sh` runs identically on Linux and Windows runners and asserts real behavior; it is the
  end-to-end guard the CI relies on. Keep assertions running on CI, not locally.
- `tools/ci-integration.sh` drives the real client → server HTTP link (push, dedup, auth, path traversal)
  and prints `INTEG OK`.
- `tools/ci-audio.sh` prints `AUDIO OK`: it synthesizes a lesson recording with Python's `wave`, then asserts
  turn segmentation, third-party-readable blobs, and that a lesson cut off mid-speech still flushes its tail
  plus `session.close` into that lesson. Linux needs `libasound2-dev` for cpal/ALSA (installed in `ci.yml`).
  The mic path itself cannot be verified on a runner, so `a-audio` treats "no input device" as an observable
  outcome (the source is simply absent from the health table) and reports `stream_errors` in `session.close`
  rather than papering over dropouts; verify the device branch on a real machine.
  Scenario ⑤ covers the teacher's whole tuning loop (POST the new threshold → next lesson reads it back from
  the adapter's own `session.open` self-report), and ⑥ asserts a blob comes back as `audio/wav` with the same
  bytes as on disk — that is what lets the observer play it back through a plain `<audio src>`.
- `tools/ci-screen.sh` prints `SCREEN OK`: it hand-writes PNGs with `zlib` + `struct` (no PIL — the adapter
  must parse a file a *third party* wrote, same reasoning as `wave` above), then asserts a 6-page deck yields
  6 frames, a 13-image still yields exactly **1** frame with `unchanged == 12`, throttling counts, that a
  lesson cut off mid-page still flushes its `trigger: "close"` tail frame plus `session.close` into that
  lesson, and the tuning loop over `screen.min_dist`. The reverse assertion is the point: "nothing changed"
  masquerading as "many changes" turns the evidence into thousands of near-identical screenshots nobody opens.
  There is no `libasound2`-style dep — the device branch is guarded as "must not crash, must not invent blob
  references", and ⑧ cross-checks the three copies of the backend list. When asserting "this source produced
  nothing", filter `events.ndjson` by `adapter_id`: the core writes its own facts into the same file
  (`core.admit` is ~1 KB per loaded source), so an unfiltered count reads "no desktop on this machine" as
  "emitted events but no close record" — two different diagnoses and two different fixes.
- `tools/ci-smoke.sh` also drives the write endpoint's four boundaries (params accepted, `argv` refused with
  the file byte-identical, reversed hysteresis refused, non-object refused) and a same-process reload guard:
  `stop` → edit `params` on disk → `start`, asserting the next lesson's segments are 8000 bytes instead of
  32000. That pair is the only proof that "next lesson takes effect" is real rather than a slogan.
- `.github/workflows/agent.yml` runs a seconds-level text guard over `agent/ui/` before building: binary
  playback (audio *and* keyframe images) must go through `/blob/` and never through the text-only `http_get`,
  the nine `a-audio` and ten `a-screen` tuning field names must match the keys those adapters actually read,
  the `capture` dropdown must equal `capture.rs::BACKENDS`, and every `#id` referenced by `app.js` must exist
  in `index.html`. A typo in any of those fails silently at runtime, so it has to be loud here.
  These guards are pure text — run them locally before pushing; a regex that never matches looks identical to
  a passing guard, and that bug was actually caught this way.
- Renaming a module means touching `ci.yml` artifact paths, `tools/*.sh`, and `docs/*.html` — grep for the
  old binary name before considering a rename done.
- `debug = 0` and `strip`/`lto` are set in the workspace `Cargo.toml` because disk size matters; do not
  reintroduce heavy debug artifacts (e.g. Windows PDBs) casually.
