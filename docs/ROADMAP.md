# Meeting Notes CRM — roadmap

M0 is this base setup; the later milestones come from the end goal (`docs/END_GOAL.md`).
A milestone's status cell says *Done* only with named tests and evidence.

| M | Spec week | Deliverable | Gate | Status |
|---|---|---|---|---|
| M0 | — | Base setup from the starter template: CI, gate, port band 63600–63699, drills and evidence kit | CI green on the empty product | **In progress — this base setup** |
| M0 Foundations | 1 | CI, recorded transcriber and model fixtures, mock HubSpot, `kill-mid-pipeline` drill, `review-api` skeleton |  | Planned |
| M1 Media and transcript | 1 | Consent-gated upload → transcode → hosted transcription with diarisation → speaker-labelled transcript of a 30-minute role-played call | `media_bounds_*`, `tests/routes.rs` (isolation), recorded-provider failure/timeout/partial/retry/deletion tests (`crates/meetcrm-meetings/tests/pipeline.rs`), `a_thirty_minute_role_played_call_becomes_a_speaker_labelled_transcript`, `scripts/prove-limits.sh`, CI job `load-meetings` (1M) | Built, in review; *Done* once CI and the load result are green |
| M2 Extraction | 2 | Evidence check, templates, date and owner resolution, eval binary; action-item precision ≥ 85% on the eval set |  | Planned |
| M3 CRM | 3 | OAuth, mapping fetch, matching, review diff, idempotent push, audit log; approved changes appear in a HubSpot test account |  | Planned |
| M4 Follow-through | 4 | Email drafts, commitment tracker, search answering "what did we promise X?" with cited segments |  | Planned |
| M5 Privacy and hardening | 5–6 | Retention and deletion drill, local whisper option if measured viable, load scenarios, docs, demo video, 3 beta teams |  | Planned |
| M6 Ship gate | after 6 | All checklist items above |  | Planned |
