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
  the only protocol, a demuxer allowlist, and `prlimit` caps on CPU time, address space
  (`MEETCRM_MEDIA_MAX_MEMORY_BYTES`, 512 MiB: the tools share the service's cgroup), file size,
  open files and core dumps, under a wall-clock deadline; one thread (`-threads 1`,
  `-filter_threads 1`) and bounded probing (`-probesize` 5 MB, `-analyzeduration` 10 s). A
  SIGKILL (the OOM killer, an operator) is `media_killed` and retried, not a CPU-limit verdict.
- **Provider port**: `meetcrm_transcribe::Transcriber`; a hosted Deepgram-shaped HTTPS adapter
  (hyper + hyper-rustls, webpki roots) and a recorded adapter for tests, demos and load. The
  production default is `hosted`; `recorded` must be chosen explicitly. A chunk is streamed
  from its file (64 KiB at a time), never read whole. A cut answer and a refused key (401,
  403) are retried with jittered backoff; only another 4xx or an answer past its bounds is
  permanent. No live key exists yet: the hosted adapter is tested against a local stand-in
  only, and the role-play answers are built from the script's timings (tracked in the roadmap).
- **Chunks**: ten-minute FLAC chunks (16 kHz mono), each transcribed and applied once; speaker
  labels are per chunk (`Speaker N (part M)`) until cross-chunk speaker matching is decided.
- **Fixture**: the 30-minute call is synthetic: a script written for the repository
  (`fixtures/roleplay/call.txt`) read by two espeak-ng voices; generated in CI, never committed.

## D-005 — Upload admission and media retention

- **Admission**: an upload claims its meeting before reading a byte (`awaiting_media` →
  `receiving`, with a claim id and a deadline); a second upload to it is 409 `media_exists`.
  It also takes a server-wide slot, one of its workspace's (`MEETCRM_UPLOADS_PER_WORKSPACE`,
  2) and one of its member's (`MEETCRM_UPLOADS_PER_USER`, 1); past any, 503 `uploads_busy`.
  The body must keep a minimum average rate (`MEETCRM_UPLOAD_MIN_BYTES_PER_S`, 16 KiB/s after
  `MEETCRM_UPLOAD_GRACE_S`, 10 s) or it is cut with 408 `upload_too_slow`, so a trickle cannot
  hold a slot for the overall deadline.
- **Plan limits**: a workspace holds at most `MEETCRM_WORKSPACE_MAX_MEETINGS` meetings and
  `MEETCRM_WORKSPACE_MAX_MEDIA_BYTES` of undeleted recordings, checked under the workspace's
  row lock with in-flight uploads' declared sizes reserved.
- **Retention**: every terminal failure (rejected media, exhausted transcode or transcription
  attempts, a permanent provider refusal) deletes chunks and partial segments and queues the
  audio for removal; the original is placed only after the database commit, and
  `audio_deleted_at` is set only once removal succeeded.
- **Authorization**: `Action::Record` (create, consent, upload, delete one's own meeting) for
  every member role; `Action::ManageMeetings` (delete another's) for owners and admins.
  Tokens stay read-only.
