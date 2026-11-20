# 08 Meeting Notes → CRM — verified knowledge

Checked on 2026-11-20 against primary sources where they exist. Every fact carries a source. **UNVERIFIED**
marks what could not be confirmed. "The spec says" marks spec numbers taken without confirmation. Prices,
limits and legal positions are dated and will drift.

## 1. Claude API for extraction

The shared facts (model IDs and prices, no official Rust SDK, API version `2026-10-21` removing `temperature`/
`top_p`/`top_k`, structured-output limits, streaming errors after HTTP 200, rate/ramp/spend limits) are in
`../06-content-repurposing/KNOWLEDGE.md` §1 with sources. Current choices: `claude-opus-5-5` ($4/$20 per MTok),
`claude-sonnet-5-5` ($2/$10), `claude-haiku-5-5` ($0.10/$0.50 up to 100K-token prompts)
([pricing](https://platform.claude.com/docs/en/about-claude/pricing)). Points specific to this project:

- **The extraction schema hits structured-output limits quickly.** Each nullable field written as
  `"type": ["string","null"]` counts toward the limit of 16 union-typed parameters, and every non-required
  field toward the limit of 24 optional parameters, summed across all strict schemas in the request
  ([structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)). The spec's
  schema (nullable `due`, optional evidence, CRM updates) should make fields required and use sentinel values.
- **Enum casing is not guaranteed** (same page): CRM field names and stage IDs returned as enums must be
  matched case-insensitively and then mapped to the exact CRM value in code.
- **Model "confidence" is not a calibrated probability.** The spec's `confidence: 0.0` field is the model's own
  number; calibration must be measured per bucket on the eval set before it gates anything (inference, no
  source claims otherwise).
- **Covered Models** (Fable 5.x, Mythos 5.x) require 30-day retention and are not ZDR-eligible; Anthropic states
  that retained data is never used for training without express permission
  ([data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention)). The default
  retention period for ordinary commercial API traffic is in a separate policy article not checked here
  (**UNVERIFIED**). For transcripts, prefer non-Covered models.

**Cost per meeting hour, illustrative.** Hosted transcription at Deepgram's pre-recorded rate is about $0.26 per
hour (section 2). Assuming a one-hour transcript of about 20,000 tokens and 3,000 output tokens, extraction with
Sonnet 5.5 is about $0.07 (arithmetic on list prices; token counts are assumptions to be measured). All-in,
roughly $0.30–0.40 per hosted meeting hour before chunk overlap and email drafting.

## 2. Hosted transcription with diarisation

| Provider | Price (pre-recorded) | Diarisation | Notes |
|---|---|---|---|
| Deepgram Nova-3 | $0.0043/min mono, $0.0052/min multilingual | included for pre-recorded on pay-as-you-go; $200 free credit | Hindi support not stated on the pricing page ([Deepgram](https://deepgram.com/pricing)) |
| AssemblyAI-STT-3.7 | $0.21/h; Universal-2 $0.15/h | +$0.02/h | $50 free credit; EU endpoints at the same price ([AssemblyAI](https://www.assemblyai.com/pricing.md)) |
| OpenAI `gpt-4o-transcribe-diarize` | $2.50/$10 per M audio tokens | yes | **marked Deprecated** ([OpenAI](https://developers.openai.com/api/docs/models/gpt-4o-transcribe-diarize.md)); the spec's example provider is a poor choice |
| Sarvam (Indian languages) | docs list ₹30/h, ₹45/h with diarisation; other Sarvam pages conflict | yes; code-mixed speech claimed | ([Sarvam pricing](https://docs.sarvam.ai/api/pricing)); figures **UNVERIFIED** |

**Hinglish accuracy: no independent benchmark found (UNVERIFIED for every provider).** The spec's own mitigation,
testing on the maintainer's own role-played recordings, is the only sound basis for choosing.

## 3. Local transcription: whisper-rs and whisper.cpp

- `whisper-rs` 0.16.0 (2026-03-12, Unlicense); the repository moved to Codeberg
  ([crates.io](https://crates.io/crates/whisper-rs), [Codeberg](https://codeberg.org/tazz4843/whisper-rs)).
  Features include `cuda`, `metal`, `vulkan`, `hipblas`, `openblas`, `coreml`, `openmp`.
- Its bundled `whisper.cpp` submodule points at **v1.8.3** (2026-01-15), while upstream is **v1.9.5**
  (2026-10-06) ([whisper.cpp releases](https://github.com/ggml-org/whisper.cpp/releases)). The bindings lag
  upstream by several releases; plan for that or pin a fork.
- whisper.cpp is MIT. Models: tiny 75 MiB / ~273 MB RAM, base 142 MiB / ~388 MB, small 466 MiB / ~852 MB,
  medium 1.5 GiB / ~2.1 GB, large 2.9 GiB / ~3.9 GB; `large-v3-turbo` is available; integer quantisation
  (e.g. Q5_0) reduces size; Silero VAD is built in; input is 16-bit WAV, converted with
  `ffmpeg -ar 16000 -ac 1 -c:a pcm_s16le` ([README](https://github.com/ggml-org/whisper.cpp)).
- **Diarisation in whisper.cpp is only experimental "tinydiarize"**: a special `small.en-tdrz` model marks
  `[SPEAKER_TURN]`; it does not label who spoke, and the published model is an English-only (`.en`) model
  (same README).
- **CPU throughput on a small droplet: UNVERIFIED.** No published figure was found for x86 with 2–4 threads on
  small/medium/large-v3-turbo; third-party claims ("6x real-time") name no hardware. The spec's target of under
  3 minutes for a 30-minute call means an end-to-end real-time factor of 0.1 or better, including extraction.
  Measure with whisper.cpp's `bench` on the target droplet before promising the local option.
- A newer alternative binding exists, `whisper-cpp-plus` 0.1.5 (VAD and streaming)
  ([crates.io](https://crates.io/crates/whisper-cpp-plus)); maturity **UNVERIFIED**.

## 4. Speaker diarisation in or near Rust

The spec says pure-Rust diarisation is immature and recommends a hosted API. Pure Rust: still true. **In-process
via FFI is now available**:

- **`sherpa-onnx` 2.0.2** (2026-11-19, Apache-2.0), the official k2-fsa Rust wrapper, exposes
  `OfflineSpeakerDiarization` with pyannote segmentation, `SpeakerEmbeddingExtractor` and `FastClusteringConfig`
  (`num_clusters`, `threshold`; their semantics are undocumented on docs.rs), plus Silero/Ten VAD and offline
  Whisper ([docs.rs](https://docs.rs/sherpa-onnx/latest/sherpa_onnx/)). Its build script **downloads prebuilt
  native libraries from GitHub releases** unless `SHERPA_ONNX_LIB_DIR` is set; reproducible CI needs a pinned,
  checksummed archive.
- Segmentation models: pyannote `segmentation-3.0` (MIT, gated), Reverb diarization v1 (licence "other";
  Rev describes it as **non-commercial**) ([HF API](https://huggingface.co/api/models/pyannote/segmentation-3.0),
  [Reverb](https://huggingface.co/Revai/reverb-diarization-v1),
  [Rev blog](https://www.rev.com/blog/speech-to-text-technology/introducing-reverb-open-source-asr-diarization)).
  pyannote `speaker-diarization-community-1` is CC-BY-4.0 (attribution required).
- Other crates: `pyannote-rs` 0.3.4 (last release 2025-09), `speakrs` 0.8.0 (2026-11), `diarization` 0.1.0
  ([crates.io](https://crates.io/crates/speakrs)). Accuracy (DER) on Hinglish calls: **UNVERIFIED** for all.
- **Diarisation is not identification.** Clustering anonymous speakers is different from matching a stored
  voiceprint to a named person; the second creates biometric data (section 9).

## 5. Evidence and grounding for action items

Three designs, strongest first:

1. **Citations over a custom content document.** Put each transcript segment in its own content block; with
   citations enabled the API does no further chunking and returns `content_block_location` block indices that
   are "guaranteed to contain valid pointers to the provided documents"; `cited_text` does not count toward
   output tokens ([citations](https://platform.claude.com/docs/en/build-with-claude/citations)). Cost: it cannot
   be combined with `output_config.format` (400); the documented pattern is one line per field, parsed in code.
2. **Structured output with segment IDs**, then a deterministic check that each quoted string is a normalised
   substring of the referenced segments and that the timestamps match.
3. **The spec's design**: model-written quotes fuzzy-matched (`strsim`) against segments within a time window.
   `strsim` 0.11.1 (MIT, last release 2024-04) offers edit-distance metrics only
   ([crates.io](https://crates.io/crates/strsim)); `rapidfuzz` 0.5.0 (MIT) is an alternative whose feature
   parity with the Python library is **UNVERIFIED**.

Pitfalls common to all three: a real quote can still not support the item ("we might send it Friday" is not a
commitment); the quote's speaker must match the proposed owner or the item must say who assigned it; chunk
overlap creates duplicates, so merge keys must be deterministic (the spec's order-independence property test).

## 6. Relative dates

`jiff` 1.0.1 (Unlicense OR MIT) is current. `Date::nth_weekday(1, Weekday::Friday)` returns the next Friday
**excluding** the date itself ([docs.rs](https://docs.rs/jiff/latest/jiff/civil/struct.Date.html)). jiff does
not parse natural language ("next Friday"); the model must return a structured interpretation (anchor, weekday,
offset) beside `due_text`, and Rust computes the date in the meeting's time zone. "Next Friday" said on a
Wednesday is ambiguous between two Fridays; that case must be shown as ambiguous, not resolved silently.

## 7. HubSpot

- **Rate limits** ([usage guidelines](https://developers.hubspot.com/docs/developer-tooling/platform/usage-guidelines)):
  public/marketplace OAuth apps 110 requests per 10 s per installing account (CRM Search excluded; the limit-
  increase add-on does not apply). Private/static-token apps: Free and Starter 100 per 10 s and 250,000 per day;
  Professional 190 / 625,000; Enterprise 190 / 1,000,000. 429 applies to all later calls; daily limits reset at
  midnight in the account's time zone; Search responses carry no rate-limit headers; marketplace apps must keep
  errors under 5% of daily requests.
- **OAuth**: access tokens last 30 minutes (`expires_in` 1800); refresh with `grant_type=refresh_token`
  ([HubSpot](https://developers.hubspot.com/docs/api-reference/latest/authentication/manage-oauth-tokens)). Deal
  writes need `crm.objects.deals.write`.
- **Distribution limits**: apps on developer platform 2025.2 that are not listed on the marketplace are capped at
  25 installs; privately distributed OAuth apps at 10 allowlisted accounts; static-token apps at one account
  ([HubSpot changelog](https://developers.hubspot.com/changelog/new-marketplace-distribution-app-install-limits),
  community threads cited there). A three-team beta fits; a public launch needs marketplace listing.
- **API versioning**: date-based paths since 2026-03-30, latest `2026-09` (e.g. `/crm/objects/2026-09/contacts`);
  all legacy v1–v4 APIs lose support in September 2027
  ([migration guide](https://developers.hubspot.com/docs/api-reference/legacy/migration-guide)), although a
  separate changelog gives 2027-03-30 for v4
  ([changelog](https://developers.hubspot.com/changelog/deprecating-support-for-hubspot-v4-apis)); the two
  conflict. Build on the date-based paths either way.
- **Deal stages are portal-specific internal IDs.** Custom stages get numeric or GUID-like IDs, and default
  labels have changed across portal ages; read `GET /crm/v3/pipelines/deals` (or its date-based equivalent)
  per portal ([HubSpot community](https://community.hubspot.com/t/value-for-deal-stage-pulled-thru-api/85724)).
  The spec's example values `qualification` → `proposal_sent` are not HubSpot IDs; the field mapping must be
  fetched, never hard-coded.
- **Free CRM**: API use is allowed (the private-app table lists a Free tier); the number of deal pipelines on
  Free (third parties say one) is **UNVERIFIED**.
- **Conditional writes**: no evidence found that CRM object updates support `If-Match` (**UNVERIFIED**). Design
  for read-compare-write with the observed value recorded, and for **non-idempotent creates** (a retried note or
  task after a timeout can duplicate): store an idempotency marker and search for it before retrying.

## 8. Follow-up email and storage

- Gmail `gmail.send` is a **sensitive** scope (OAuth verification required); restricted scopes such as
  `gmail.readonly`/`gmail.modify` add a CASA security assessment
  ([Google](https://developers.google.com/identity/protocols/oauth2/production-readiness/restricted-scope-verification),
  [Nylas guide](https://developer.nylas.com/docs/provider-guides/google/google-verification-security-assessment-guide/)).
  Copy-to-clipboard avoids both.
- Object storage lifecycle: R2 deletes expired objects "typically within 24 hours"
  ([R2](https://developers.cloudflare.com/r2/buckets/object-lifecycles)); Spaces supports time-based expiration
  only ([DigitalOcean](https://docs.digitalocean.com/products/spaces/how-to/configure-lifecycle-rules/)). A
  lifecycle rule is a backstop, not the deletion mechanism the privacy promise rests on.
- Search: Neon supports pgvector on every plan, HNSW and IVFFlat; keep `maintenance_work_mem` under 50–60% of
  compute RAM for HNSW builds ([Neon](https://neon.com/docs/extensions/pgvector)). `fastembed` 7.7.1
  (Apache-2.0) offers multilingual models (`multilingual-e5-*`, `bge-m3`) and **downloads them from Hugging Face
  on first use** ([fastembed-rs](https://github.com/Anush008/fastembed-rs)); bake them into the image.
- Crate versions on 2026-11-20: `aes-gcm` 0.11.1, `oauth2` 5.0.0, `aws-sdk-s3` 1.161.0, `object_store` 0.14.2,
  `pgvector` 0.4.2, `sqlx` 0.9.1, `axum` 0.8.9, `tokio` 1.54.1 ([crates.io](https://crates.io/)).

## 9. Recording consent and privacy: constraints on a public demo (not legal advice)

- **United States**: federal law is one-party consent (18 U.S.C. § 2511(2)(d)); all-party states per a 2024
  chart include California, Connecticut, Delaware, Florida, Illinois, Maryland, Massachusetts, Montana and
  Nevada, with Pennsylvania and Washington also listed; for interstate calls the stricter rule is generally
  assumed ([Kilpatrick chart](https://ktslaw.com/en/Blog/GlobalPrivacy-and-CybersecurityLaw/2024/7/Wiretap-Laws-in-the-United-States),
  [Justia survey](https://justia.com/50-state-surveys/recording-phone-calls-and-conversations)). New
  Hampshire's status was not confirmed (**UNVERIFIED**).
- **AI notetaker litigation**: *In re Otter.AI Privacy Litigation* (N.D. Cal., 5:25-cv-06911) alleges Wiretap
  Act and CIPA violations, including a bot that joined calls and recorded without all parties' consent; one
  report says core claims survived a motion to dismiss in August 2026
  ([NPR](https://www.npr.org/2025/08/15/g-s1-83087/otter-ai-transcription-class-action-lawsuit),
  [UC Today](https://uctoday.com/productivity-automation/otter-ai-fails-to-dismiss-core-privacy-claims-in-u-s-court/);
  ruling **UNVERIFIED** against the docket). The spec's stretch goal "live meeting bot" is the exact pattern
  in dispute.
- **Voiceprints are biometric identifiers under Illinois BIPA.** *Cruz v. Fireflies.AI* (filed 2025-12-18) and
  a 2026 follow-on allege speaker recognition built voiceprints without written release or a published
  retention policy ([Epstein Becker Green](https://www.ebglaw.com/insights/publications/ai-meeting-assistants-and-biometric-privacy-lessons-from-the-fireflies-ai-lawsuit),
  [Fisher Phillips](https://www.fisherphillips.com/en/insights/insights/ai-meeting-tools-are-the-latest-target-of-illinois-bipa-class-actions)).
  Owner resolution must use names from the transcript and a roster confirmed by a person, never stored voice
  embeddings tied to people.
- **EU/UK GDPR**: recordings and transcripts are personal data; biometric data processed to uniquely identify a
  person is special-category data ([Art. 4(14)](https://gdpr-info.eu/art-4-gdpr/),
  [Art. 9](https://gdpr-info.eu/art-9-gdpr/)).
- **India**: DPDP Rules 2025 were notified on 13/14 November 2025; consent-manager rules apply from November 2026
  and the core duties (notice, consent, breach reporting, retention) from 13 May 2027
  ([PIB](https://static.pib.gov.in/WriteReadData/specificdocs/documents/2025/nov/doc20251117695301.pdf),
  [Mondaq](https://webiis10.mondaq.com/india/data-protection/1706878/indias-digital-personal-data-protection-act-phased-rollout-and-key-compliance-milestones)).
  Recording-specific rules for India and Canada were not researched (**UNVERIFIED**).
- **What a public demo may do**: process only synthetic or role-played recordings the maintainer made with
  every participant's consent (the spec's own eval rule); require a consent attestation per upload, stored with
  the meeting; no meeting bot; no voiceprints; delete audio after transcription by default; write only to a
  HubSpot developer test account; never accept real customer recordings on the public instance.
