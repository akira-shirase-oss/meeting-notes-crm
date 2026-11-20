# Decisions

Trade-offs and decisions, D-001 onward. Each entry says what was chosen and why; a
superseded decision is marked, never rewritten.

## D-001 — Base: the Backend Platform starter template

The repository begins as a copy of the Backend Platform's copy-and-own starter template —
the bounded HTTP layer, the three-role PostgreSQL split with its protections manifest,
the accounts kit, releases, restore drills, and the CI + evidence kit — renamed for this
product. The commit it came from is in `TEMPLATE_ORIGIN`; later template changes arrive
only as reviewed diffs. Nothing shares a package, service, login or database with the
template's repository or any sibling product.

## D-002 — Port band 63600-63699

Every port this repository binds on a shared development machine stays inside
63600–63699: the server on 63650, the development PostgreSQL on 63632, the
template's other defaults shifted into the band. `scripts/check-ports.sh` fails on a
literal outside the band, and the gate proves it can fail. Rationale: Linux's ephemeral
range ends at 60999, so CI and a second checkout never collide.

## D-003 — Stack

Rust backend on Axum, SQLx and tokio (the template's stack); SvelteKit + TypeScript
(strict) for the web UI once the maintainer chooses this product's design system — no
`web/` product code before that.
Everything else follows `docs/END_GOAL.md`.

## D-004 — M1 media and transcription

- **ffmpeg**: the static FFmpeg 7.0.2 amd64 build, pinned by SHA-256 (`scripts/install-ffmpeg.sh`);
  never a system ffmpeg. It runs with argv only (no shell), a cleared environment, `file:` as
  the only protocol, a demuxer allowlist, and `prlimit` caps on CPU time, file size, open files
  and core dumps, under a wall-clock deadline.
- **Provider port**: `meetcrm_transcribe::Transcriber`; a hosted Deepgram-shaped HTTPS adapter
  (hyper + hyper-rustls, webpki roots) and a recorded adapter for tests, demos and load. No
  live key exists yet: the hosted adapter is tested against a local stand-in only.
- **Chunks**: ten-minute FLAC chunks (16 kHz mono), each transcribed and applied once; speaker
  labels are per chunk (`Speaker N (part M)`) until cross-chunk speaker matching is decided.
- **Fixture**: the 30-minute call is synthetic: a script written for the repository
  (`fixtures/roleplay/call.txt`) read by two espeak-ng voices; generated in CI, never committed.

