//! The M1 pipeline end to end, in process: consent-gated upload, probe, transcode with the
//! pinned ffmpeg, transcription through the recorded provider, the transcript, deletion;
//! and every failure path `docs/END_GOAL.md` names for M1 (failure cases 11 and 15).
#![expect(clippy::unwrap_used, reason = "integration test helpers")]

mod common;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::http::{Method, StatusCode};
use common::{Kit, answer, sha_hex, tone};
use meetcrm_media::tools::Tools;
use meetcrm_meetings::worker::Outcome;
use meetcrm_transcribe::recorded::{ANY, Fault, Recorded};
use meetcrm_transcribe::{ChunkAudio, TranscribeError, Transcriber, Transcription};
use tokio::sync::Notify;

const SEGMENTS: &str = "SELECT count(*) FROM transcript_segments WHERE meeting_id = $1";

/// A body that never yields, and says whether it was ever polled.
struct Untouched(Arc<AtomicBool>);

impl HttpBody for Untouched {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        self.0.store(true, Ordering::SeqCst);
        Poll::Pending
    }
}

fn untouched() -> (Body, Arc<AtomicBool>) {
    let polled = Arc::new(AtomicBool::new(false));
    (Body::new(Untouched(polled.clone())), polled)
}

/// Saves a two-speaker answer for each two-second chunk of a four-second tone.
fn record_two_chunks(kit: &Kit, recording: &str) {
    kit.record(
        recording,
        0,
        &answer(
            2.0,
            &[
                (0, 0.1, 0.9, "Thanks for making time today."),
                (1, 1.0, 1.9, "Happy to."),
            ],
        ),
    );
    kit.record(
        recording,
        1,
        &answer(
            2.0,
            &[
                (1, 0.2, 1.0, "What does the renewal cost?"),
                (0, 1.1, 1.8, "I will send it."),
            ],
        ),
    );
}

#[tokio::test]
async fn a_consented_recording_becomes_a_speaker_labelled_transcript_and_its_audio_is_deleted() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    let recording = sha_hex(&audio);
    record_two_chunks(&kit, &recording);
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.show(&ana, meeting).await.body["status"], "transcoding");
    assert!(kit.files().await >= 1);

    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.show(&ana, meeting).await.body["chunks"], 2);
    assert_eq!(
        kit.transcript(&ana, meeting).await.code(),
        Some("transcript_incomplete")
    );
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.work().await, Outcome::Idle);

    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "transcribed");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.files().await, 0, "audio is deleted once transcribed");
    let t = kit.transcript(&ana, meeting).await;
    assert_eq!(t.status, StatusCode::OK, "{}", t.body);
    let segments = t.body["segments"].as_array().unwrap();
    let got: Vec<(&str, i64, &str)> = segments
        .iter()
        .map(|s| {
            (
                s["speaker"].as_str().unwrap(),
                s["start_ms"].as_i64().unwrap(),
                s["text"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("Speaker 1 (part 1)", 100, "Thanks for making time today."),
            ("Speaker 2 (part 1)", 1000, "Happy to."),
            ("Speaker 2 (part 2)", 2216, "What does the renewal cost?"),
            ("Speaker 1 (part 2)", 3116, "I will send it."),
        ]
    );
    assert_eq!(kit.recorded.calls(), 2);
}

#[tokio::test]
async fn media_bounds_upload_without_consent_is_refused_before_any_byte_is_read() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    let (body, polled) = untouched();
    let refused =
        tokio::time::timeout(Duration::from_mins(1), kit.upload_body(&ana, meeting, body))
            .await
            .expect("answered without reading the body");
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.code(), Some("consent_required"));
    assert!(!polled.load(Ordering::SeqCst), "the body was never read");
    assert_eq!(kit.files().await, 0);
    assert_eq!(
        kit.show(&ana, meeting).await.body["status"],
        "awaiting_consent"
    );
}

#[tokio::test]
async fn media_bounds_corrupt_file_is_refused_and_nothing_is_stored() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    kit.consent(&ana, meeting).await;
    let refused = kit
        .upload(&ana, meeting, b"RIFF\0\0\0\0WAVEjunk not audio".to_vec())
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.body
    );
    assert_eq!(refused.code(), Some("media_corrupt"));
    assert_eq!(kit.files().await, 0);
    assert_eq!(
        kit.show(&ana, meeting).await.body["status"],
        "awaiting_media"
    );
    let empty = kit.upload(&ana, meeting, Vec::new()).await;
    assert_eq!(empty.code(), Some("media_corrupt"));
    // A good recording is still accepted afterwards.
    let ok = kit.upload(&ana, meeting, tone(1)).await;
    assert_eq!(ok.status, StatusCode::ACCEPTED, "{}", ok.body);
}

/// A one-second Ogg Opus file whose last page claims six hours: the container's own
/// duration, which ffprobe reports without decoding.
fn six_hour_ogg() -> Vec<u8> {
    ogg_claiming(6 * 3600)
}

/// One second of Opus in Ogg whose last page claims `seconds` (granule patched, CRC fixed).
fn ogg_claiming(seconds: u64) -> Vec<u8> {
    let dir = common::scratch("ogg");
    let path = dir.join("call.ogg");
    let status = std::process::Command::new(common::bin_dir().join("ffmpeg"))
        .args([
            "-v",
            "quiet",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=1",
        ])
        .args(["-c:a", "libopus", "-b:a", "16k", "-f", "ogg"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let mut ogg = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    let last = ogg.windows(4).rposition(|w| w == b"OggS").unwrap();
    let granule: u64 = seconds * 48_000;
    ogg[last + 6..last + 14].copy_from_slice(&granule.to_le_bytes());
    ogg[last + 22..last + 26].fill(0);
    let crc = ogg_crc(&ogg[last..]);
    ogg[last + 22..last + 26].copy_from_slice(&crc.to_le_bytes());
    ogg
}

/// The Ogg page checksum: CRC-32, polynomial 0x04c11db7, unreflected, zero start.
fn ogg_crc(page: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in page {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04c1_1db7
            };
        }
    }
    crc
}

#[tokio::test]
async fn media_bounds_six_hour_file_is_refused_and_nothing_is_stored() {
    let kit = Kit::tuned(|s| s.media.max_duration = Duration::from_hours(4)).await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    kit.consent(&ana, meeting).await;
    let refused = kit.upload(&ana, meeting, six_hour_ogg()).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.body
    );
    assert_eq!(refused.code(), Some("media_too_long"));
    assert_eq!(kit.files().await, 0);

    let short = Kit::tuned(|s| s.media.max_duration = Duration::from_secs(3)).await;
    let bea = short.person("bea@example.test").await;
    let meeting = short.meeting(&bea).await;
    short.consent(&bea, meeting).await;
    let refused = short.upload(&bea, meeting, tone(6)).await;
    assert_eq!(refused.code(), Some("media_too_long"));
    assert_eq!(short.files().await, 0);
}

#[tokio::test]
async fn media_bounds_oversized_upload_is_refused_by_length_and_while_streaming() {
    let kit = Kit::tuned(|s| s.media.max_upload_bytes = 64 * 1024).await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    kit.consent(&ana, meeting).await;
    let declared = kit.upload(&ana, meeting, vec![0; 64 * 1024 + 1]).await;
    assert_eq!(
        declared.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        declared.body
    );
    assert_eq!(kit.files().await, 0);
    // No declared length: the store's cap stops it mid-stream.
    let streamed = kit.upload_body(&ana, meeting, Body::new(Zeros(20))).await;
    assert_eq!(
        streamed.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        streamed.body
    );
    assert_eq!(kit.files().await, 0);
}

/// A body of `n` 8 KiB frames of zeros, with no declared length.
struct Zeros(u32);

impl HttpBody for Zeros {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        if self.0 == 0 {
            return Poll::Ready(None);
        }
        self.0 -= 1;
        Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from(vec![
            0u8;
            8 * 1024
        ])))))
    }
}

#[tokio::test]
async fn media_bounds_a_stalled_upload_is_cut_at_the_idle_deadline() {
    let kit = Kit::tuned(|s| s.upload_idle = Duration::from_millis(200)).await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    kit.consent(&ana, meeting).await;
    let (body, polled) = untouched();
    let cut = tokio::time::timeout(
        Duration::from_secs(30),
        kit.upload_body(&ana, meeting, body),
    )
    .await
    .unwrap();
    assert!(polled.load(Ordering::SeqCst));
    assert_eq!(cut.status, StatusCode::REQUEST_TIMEOUT, "{}", cut.body);
    assert_eq!(cut.code(), Some("upload_stalled"));
    assert_eq!(kit.files().await, 0);
}

#[tokio::test]
async fn media_bounds_ffmpeg_that_hangs_is_killed_at_the_deadline_and_nothing_is_kept() {
    let dir = common::scratch("hang");
    let hang = dir.join("ffmpeg");
    std::fs::write(&hang, "#!/bin/sh\nsleep 600\n").unwrap();
    std::fs::set_permissions(&hang, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let tools = Tools {
        ffmpeg: hang,
        ..common::tools()
    };
    let kit = Kit::with(
        |s| s.media.transcode_deadline = Duration::from_millis(500),
        None,
        Some(tools),
    )
    .await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, tone(1)).await;
    assert_eq!(kit.work().await, Outcome::Rejected("media_deadline"));
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "media_rejected");
    assert_eq!(shown["reason"], "media_deadline");
    assert_eq!(kit.files().await, 0);
    assert_eq!(kit.work().await, Outcome::Idle);
}

#[tokio::test]
async fn provider_down_shows_transcription_unavailable_and_retries_within_bounds() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(1);
    let recording = sha_hex(&audio);
    kit.record(&recording, 0, &answer(1.0, &[(0, 0.0, 0.5, "Hello.")]));
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    for _ in 0..3 {
        kit.recorded
            .fault(ANY, Fault::Fail(TranscribeError::Unavailable));
    }
    assert_eq!(kit.work().await, Outcome::Retrying("provider_unavailable"));
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "transcription_unavailable");
    assert_eq!(shown["reason"], "provider_unavailable");
    assert_eq!(
        kit.transcript(&ana, meeting).await.code(),
        Some("transcript_incomplete")
    );
    assert_eq!(kit.work().await, Outcome::Retrying("provider_unavailable"));
    // Third attempt of three: out of attempts.
    assert_eq!(kit.work().await, Outcome::Failed("provider_unavailable"));
    assert_eq!(kit.work().await, Outcome::Idle);
    assert_eq!(kit.recorded.calls(), 3);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 0);
    assert_eq!(
        kit.show(&ana, meeting).await.body["status"],
        "transcription_unavailable"
    );
    // Over quota is the same: retried, never complete.
    let quota = Kit::new().await;
    let bea = quota.person("bea@example.test").await;
    let audio = tone(1);
    quota.record(
        &sha_hex(&audio),
        0,
        &answer(1.0, &[(0, 0.0, 0.5, "Hello.")]),
    );
    let meeting = quota.uploaded(&bea, audio).await;
    assert_eq!(quota.work().await, Outcome::Transcoded);
    quota
        .recorded
        .fault(ANY, Fault::Fail(TranscribeError::RateLimited));
    assert_eq!(
        quota.work().await,
        Outcome::Retrying("provider_rate_limited")
    );
    assert_eq!(quota.work().await, Outcome::Transcribed);
    assert_eq!(
        quota.show(&bea, meeting).await.body["status"],
        "transcribed"
    );
}

#[tokio::test]
async fn a_provider_that_never_answers_is_cut_at_the_deadline_and_retried() {
    let kit = Kit::tuned(|s| s.provider_deadline = Duration::from_millis(300)).await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(1);
    kit.record(
        &sha_hex(&audio),
        0,
        &answer(1.0, &[(0, 0.0, 0.5, "Hello.")]),
    );
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    kit.recorded.fault(ANY, Fault::Hang);
    let worked = tokio::time::timeout(Duration::from_secs(30), kit.work()).await;
    assert_eq!(worked.unwrap(), Outcome::Retrying("provider_timeout"));
    assert_eq!(
        kit.show(&ana, meeting).await.body["reason"],
        "provider_timeout"
    );
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 1);
}

#[tokio::test]
async fn a_partial_answer_is_never_stored_or_marked_complete() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    let recording = sha_hex(&audio);
    record_two_chunks(&kit, &recording);
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    // Chunk 1's answer covers 0.5 s of its 2 s.
    kit.recorded.fault(
        &Recorded::key(&recording, 1),
        Fault::Answer(answer(0.5, &[(0, 0.0, 0.4, "What")])),
    );
    assert_eq!(kit.work().await, Outcome::Retrying("provider_partial"));
    assert_eq!(
        kit.show(&ana, meeting).await.body["status"],
        "transcription_unavailable"
    );
    assert_eq!(
        kit.transcript(&ana, meeting).await.code(),
        Some("transcript_incomplete")
    );
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM transcript_segments WHERE meeting_id = $1 AND chunk = 1",
            meeting
        )
        .await,
        0,
        "nothing of the partial answer is kept"
    );
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 4);
}

#[tokio::test]
async fn a_retried_job_applies_each_chunk_exactly_once() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    let recording = sha_hex(&audio);
    record_two_chunks(&kit, &recording);
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    kit.recorded.fault(
        &Recorded::key(&recording, 1),
        Fault::Fail(TranscribeError::Unavailable),
    );
    assert_eq!(kit.work().await, Outcome::Retrying("provider_unavailable"));
    assert_eq!(kit.count(SEGMENTS, meeting).await, 2, "chunk 0 is stored");
    assert_eq!(kit.work().await, Outcome::Transcribed);
    // Chunk 0 was not asked for again, and nothing is doubled.
    assert_eq!(kit.recorded.calls(), 3);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 4);
    let t = kit.transcript(&ana, meeting).await.body;
    assert_eq!(t["segments"].as_array().unwrap().len(), 4);
}

/// Answers from the recorded adapter, but the first call waits until released.
struct Gate {
    inner: Arc<Recorded>,
    entered: Notify,
    release: Notify,
    first: AtomicBool,
}

impl Transcriber for Gate {
    fn transcribe<'a>(&'a self, chunk: ChunkAudio<'a>) -> Transcription<'a> {
        Box::pin(async move {
            if !self.first.swap(true, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.transcribe(chunk).await
        })
    }
}

#[tokio::test]
async fn a_worker_whose_lease_was_taken_over_writes_nothing() {
    let answers = common::scratch("gate");
    let recorded = Arc::new(Recorded::new(&answers));
    let gate = Arc::new(Gate {
        inner: recorded.clone(),
        entered: Notify::new(),
        release: Notify::new(),
        first: AtomicBool::new(false),
    });
    let kit = Kit::with(|_| {}, Some(gate.clone()), None).await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(1);
    let recording = sha_hex(&audio);
    let path = recorded.answer_path(&recording, 0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        answer(1.0, &[(0, 0.0, 0.5, "Hello."), (1, 0.6, 0.9, "Hi.")]),
    )
    .unwrap();
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);

    // Worker A claims the job and stalls inside the provider call.
    let a = {
        let meetings = kit.meetings.clone();
        tokio::spawn(async move { meetings.work_once().await.unwrap() })
    };
    gate.entered.notified().await;
    // Its lease runs out (as if it had died); worker B takes the job over and finishes.
    sqlx::query("UPDATE jobs SET lease_until = now() - interval '1 second' WHERE meeting_id = $1 AND state = 'running'")
        .bind(meeting)
        .execute(&kit.db.pool)
        .await
        .unwrap();
    assert_eq!(kit.work().await, Outcome::Transcribed);
    gate.release.notify_one();
    assert_eq!(a.await.unwrap(), Outcome::LostLease);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 2);
    assert_eq!(kit.show(&ana, meeting).await.body["status"], "transcribed");
}

#[tokio::test]
async fn deleting_a_recording_removes_its_media_and_transcript() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    record_two_chunks(&kit, &sha_hex(&audio));
    let done = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.work().await, Outcome::Transcribed);
    // A second meeting, deleted with its media still on disk.
    let pending = kit.uploaded(&ana, tone(1)).await;
    assert!(kit.files().await >= 1);

    for meeting in [done, pending] {
        let path = format!("/v1/workspaces/{}/meetings/{meeting}", ana.workspace);
        let gone = kit
            .call(Method::DELETE, &path, Some(&ana.session), None)
            .await;
        assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.body);
        assert_eq!(kit.show(&ana, meeting).await.status, StatusCode::NOT_FOUND);
        assert_eq!(
            kit.transcript(&ana, meeting).await.status,
            StatusCode::NOT_FOUND
        );
        let again = kit
            .call(Method::DELETE, &path, Some(&ana.session), None)
            .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
        for table in [
            "SELECT count(*) FROM transcript_segments WHERE meeting_id = $1",
            "SELECT count(*) FROM media_chunks WHERE meeting_id = $1",
            "SELECT count(*) FROM consents WHERE meeting_id = $1",
            "SELECT count(*) FROM jobs WHERE meeting_id = $1",
            "SELECT count(*) FROM media_garbage WHERE meeting_id = $1",
        ] {
            assert_eq!(kit.count(table, meeting).await, 0, "{table}");
        }
    }
    assert_eq!(kit.files().await, 0);
    assert_eq!(kit.work().await, Outcome::Idle);
}

#[tokio::test]
async fn deleting_the_workspace_queues_and_removes_its_media() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, tone(1)).await;
    assert!(kit.files().await >= 1);
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ana.workspace)
        .execute(&kit.db.pool)
        .await
        .unwrap();
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM media_garbage WHERE meeting_id = $1",
            meeting
        )
        .await,
        1
    );
    assert_eq!(kit.meetings.sweep().await.unwrap(), 1);
    assert_eq!(kit.files().await, 0);
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM media_garbage WHERE meeting_id = $1",
            meeting
        )
        .await,
        0
    );
}

#[tokio::test]
async fn uploads_past_the_slot_limit_are_refused_busy() {
    let kit = Kit::tuned(|s| {
        s.uploads_at_once = 1;
        s.upload_idle = Duration::from_mins(10);
    })
    .await;
    let ana = kit.person("ana@example.test").await;
    let first = kit.meeting(&ana).await;
    kit.consent(&ana, first).await;
    let second = kit.meeting(&ana).await;
    kit.consent(&ana, second).await;
    let (body, polled) = untouched();
    let kit = Arc::new(kit);
    let ana = Arc::new(ana);
    let held = {
        let (kit, ana) = (kit.clone(), ana.clone());
        tokio::spawn(async move { kit.upload_body(&ana, first, body).await })
    };
    while !polled.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    let busy = kit.upload(&ana, second, tone(1)).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code(), Some("uploads_busy"));
    assert!(busy.headers.contains_key("retry-after"));
    held.abort();
}

/// Holds an upload of `meeting` open (its body never yields) until the handle is aborted.
async fn hold_upload(
    kit: &Arc<Kit>,
    who: &Arc<common::Person>,
    meeting: uuid::Uuid,
) -> tokio::task::JoinHandle<common::Answer> {
    let (body, polled) = untouched();
    let held = {
        let (kit, who) = (kit.clone(), who.clone());
        tokio::spawn(async move { kit.upload_body(&who, meeting, body).await })
    };
    while !polled.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    held
}

async fn consented(kit: &Kit, who: &common::Person) -> uuid::Uuid {
    let meeting = kit.meeting(who).await;
    kit.consent(who, meeting).await;
    meeting
}

// Attack: parallel uploads to one consented meeting. The first claims it before a byte is
// read; the second is refused at once instead of taking a slot.
#[tokio::test]
async fn a_second_upload_to_a_meeting_is_refused_while_the_first_streams() {
    let kit = Arc::new(Kit::tuned(|s| s.upload_idle = Duration::from_mins(10)).await);
    let ana = Arc::new(kit.person("ana@example.test").await);
    let meeting = consented(&kit, &ana).await;
    let held = hold_upload(&kit, &ana, meeting).await;
    let second = kit.upload(&ana, meeting, tone(1)).await;
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.body);
    assert_eq!(second.code(), Some("media_exists"));
    assert_eq!(kit.show(&ana, meeting).await.body["status"], "receiving");
    held.abort();
    let _ = held.await;
    // The abandoned claim is released: the meeting takes a recording again.
    let mut status = String::new();
    for _ in 0..200 {
        status = kit.show(&ana, meeting).await.body["status"]
            .as_str()
            .unwrap()
            .to_owned();
        if status == "awaiting_media" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(status, "awaiting_media");
    let again = kit.upload(&ana, meeting, tone(1)).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
}

// Attack: one member opens slow uploads to take every server-wide slot. Their second is
// refused (one per person), and another workspace still uploads.
#[tokio::test]
async fn one_member_cannot_take_every_upload_slot() {
    let kit = Arc::new(Kit::tuned(|s| s.upload_idle = Duration::from_mins(10)).await);
    let ana = Arc::new(kit.person("ana@example.test").await);
    let ben = kit.person("ben@example.test").await;
    let first = consented(&kit, &ana).await;
    let second = consented(&kit, &ana).await;
    let held = hold_upload(&kit, &ana, first).await;
    let busy = kit.upload(&ana, second, tone(1)).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code(), Some("uploads_busy"));
    assert_eq!(
        kit.show(&ana, second).await.body["status"],
        "awaiting_media"
    );
    let other = consented(&kit, &ben).await;
    let accepted = kit.upload(&ben, other, tone(1)).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.body);
    held.abort();
}

// Attack: the members of one workspace together take every slot. Past the workspace's
// share they are refused; another workspace still uploads.
#[tokio::test]
async fn one_workspace_cannot_take_every_upload_slot() {
    let kit = Arc::new(Kit::tuned(|s| s.upload_idle = Duration::from_mins(10)).await);
    let ana = Arc::new(kit.person("ana@example.test").await);
    let cal = Arc::new(kit.join(&ana, "cal@example.test", "member").await);
    let dee = kit.join(&ana, "dee@example.test", "member").await;
    let ben = kit.person("ben@example.test").await;
    let a = consented(&kit, &ana).await;
    let c = consented(&kit, &cal).await;
    let d = consented(&kit, &dee).await;
    let held = [
        hold_upload(&kit, &ana, a).await,
        hold_upload(&kit, &cal, c).await,
    ];
    let busy = kit.upload(&dee, d, tone(1)).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code(), Some("uploads_busy"));
    let other = consented(&kit, &ben).await;
    let accepted = kit.upload(&ben, other, tone(1)).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.body);
    for h in held {
        h.abort();
    }
}

/// A body that sends one byte every 100 ms, forever.
struct Trickle(Option<std::pin::Pin<Box<tokio::time::Sleep>>>);

impl HttpBody for Trickle {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        let sleep = self
            .0
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(100))));
        match sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                self.0 = None;
                Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(b"R")))))
            }
        }
    }
}

// Attack: a body that trickles a byte now and then never trips a per-read idle timer; it
// is cut once it falls below the minimum rate, and the meeting is free again.
#[tokio::test]
async fn a_trickled_upload_is_cut_below_the_minimum_rate() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = consented(&kit, &ana).await;
    let cut = tokio::time::timeout(
        Duration::from_secs(60),
        kit.upload_body(&ana, meeting, Body::new(Trickle(None))),
    )
    .await
    .expect("a trickle held the upload open");
    assert_eq!(cut.status, StatusCode::REQUEST_TIMEOUT, "{}", cut.body);
    assert_eq!(cut.code(), Some("upload_too_slow"));
    assert_eq!(kit.files().await, 0);
    assert_eq!(
        kit.show(&ana, meeting).await.body["status"],
        "awaiting_media"
    );
}

const CHUNKS: &str = "SELECT count(*) FROM media_chunks WHERE meeting_id = $1";

/// Makes the meeting's job look like its worker died on it on its last attempt.
async fn die_on_last_attempt(kit: &Kit, meeting: uuid::Uuid) {
    sqlx::query(
        "UPDATE jobs SET state = 'running', lease = gen_random_uuid(), \
         lease_until = now() - interval '1 second', attempts = 3 \
         WHERE meeting_id = $1 AND state <> 'done'",
    )
    .bind(meeting)
    .execute(&kit.db.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn failure_cleanup_a_transcode_whose_worker_kept_dying_keeps_no_audio() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, tone(1)).await;
    die_on_last_attempt(&kit, meeting).await;
    assert_eq!(kit.work().await, Outcome::Idle);
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "media_rejected");
    assert_eq!(shown["reason"], "attempts_exhausted");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.files().await, 0);
}

#[tokio::test]
async fn failure_cleanup_a_transcription_whose_worker_kept_dying_keeps_no_audio_or_partial_text() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    let recording = sha_hex(&audio);
    kit.record(&recording, 0, &answer(2.0, &[(0, 0.1, 0.9, "Partial.")]));
    kit.recorded.fault(
        &meetcrm_transcribe::recorded::Recorded::key(&recording, 1),
        Fault::Hang,
    );
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    // Chunk 0 is stored, then the worker "dies" before chunk 1.
    let kit = Arc::new(kit);
    let worker = {
        let kit = kit.clone();
        tokio::spawn(async move { kit.work().await })
    };
    while kit.count(SEGMENTS, meeting).await == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    worker.abort();
    let _ = worker.await;
    die_on_last_attempt(&kit, meeting).await;
    assert_eq!(kit.work().await, Outcome::Idle);
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "transcription_unavailable");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 0);
    assert_eq!(kit.count(CHUNKS, meeting).await, 0);
    assert_eq!(kit.files().await, 0);
}

#[tokio::test]
async fn failure_cleanup_a_transcode_failing_transiently_past_its_attempts_ends_and_keeps_no_audio()
{
    let tools = Tools {
        ffmpeg: std::path::PathBuf::from("/nonexistent/ffmpeg"),
        ..common::tools()
    };
    let kit = Kit::with(|_| {}, None, Some(tools)).await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, tone(1)).await;
    assert_eq!(kit.work().await, Outcome::Retrying("media_unavailable"));
    assert_eq!(kit.show(&ana, meeting).await.body["status"], "transcoding");
    assert_eq!(kit.work().await, Outcome::Retrying("media_unavailable"));
    assert_eq!(kit.work().await, Outcome::Failed("media_unavailable"));
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "media_unavailable");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.files().await, 0);
    assert_eq!(kit.work().await, Outcome::Idle);
}

#[tokio::test]
async fn failure_cleanup_a_permanent_transcription_failure_keeps_no_audio_or_partial_text() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(4);
    let recording = sha_hex(&audio);
    kit.record(&recording, 0, &answer(2.0, &[(0, 0.1, 0.9, "Partial.")]));
    kit.recorded.fault(
        &meetcrm_transcribe::recorded::Recorded::key(&recording, 1),
        Fault::Fail(TranscribeError::Rejected),
    );
    let meeting = kit.uploaded(&ana, audio.clone()).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.work().await, Outcome::Failed("provider_rejected"));
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "transcription_unavailable");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 0);
    assert_eq!(kit.count(CHUNKS, meeting).await, 0);
    assert_eq!(kit.files().await, 0);
    // The way back: the same meeting takes the recording again, and this time completes.
    kit.record(&recording, 1, &answer(2.0, &[(1, 0.2, 1.0, "Rest.")]));
    let again = kit.upload(&ana, meeting, audio).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.count(SEGMENTS, meeting).await, 2);
}

#[tokio::test]
async fn failure_cleanup_audio_is_marked_deleted_only_once_its_files_are_gone() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(1);
    kit.record(
        &sha_hex(&audio),
        0,
        &answer(1.0, &[(0, 0.0, 0.5, "Hello.")]),
    );
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    // The meeting's directory cannot be unlinked from its parent.
    let parent = kit.media.join("media").join(ana.workspace.to_string());
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.show(&ana, meeting).await.body["audio_deleted"], false);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(kit.meetings.sweep().await.unwrap(), 1);
    assert_eq!(kit.show(&ana, meeting).await.body["audio_deleted"], true);
    assert_eq!(kit.files().await, 0);
}

#[tokio::test]
async fn failure_cleanup_an_original_that_cannot_be_placed_leaves_no_file_and_no_stuck_meeting() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = consented(&kit, &ana).await;
    let media = kit.media.join("media");
    std::fs::set_permissions(&media, std::fs::Permissions::from_mode(0o555)).unwrap();
    let refused = kit.upload(&ana, meeting, tone(1)).await;
    std::fs::set_permissions(&media, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        refused.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        refused.body
    );
    assert_eq!(kit.files().await, 0);
    // The record committed first; the worker finds no recording and ends it.
    assert_eq!(kit.work().await, Outcome::Failed("media_missing"));
    let shown = kit.show(&ana, meeting).await.body;
    assert_eq!(shown["status"], "media_unavailable");
    assert_eq!(shown["audio_deleted"], true);
    assert_eq!(kit.work().await, Outcome::Idle);
}

// A page's text is bounded in bytes as well as segments: 300 segments of 4,000 bytes do
// not come back in one ~1.2 MB page; the page stops under 1 MiB and the cursor resumes
// exactly where it stopped, losing and repeating nothing.
#[tokio::test]
async fn a_transcript_page_stops_at_its_byte_budget() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let audio = tone(1);
    let text = "a".repeat(4_000);
    let utterances: Vec<(u32, f64, f64, &str)> = (0..300)
        .map(|i| {
            let start = f64::from(i) * 0.003;
            (0, start, start + 0.002, text.as_str())
        })
        .collect();
    kit.record(&sha_hex(&audio), 0, &answer(1.0, &utterances));
    let meeting = kit.uploaded(&ana, audio).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.work().await, Outcome::Transcribed);
    let path = format!(
        "/v1/workspaces/{}/meetings/{meeting}/transcript?limit=1000",
        ana.workspace
    );
    let first = kit.call(Method::GET, &path, Some(&ana.session), None).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let page = first.body["segments"].as_array().unwrap().len();
    assert_eq!(page, 1024 * 1024 / 4_000);
    let next = first.body["next"].as_i64().unwrap();
    let rest = kit
        .call(
            Method::GET,
            &format!("{path}&after={next}"),
            Some(&ana.session),
            None,
        )
        .await;
    assert_eq!(rest.status, StatusCode::OK, "{}", rest.body);
    assert_eq!(rest.body["segments"].as_array().unwrap().len(), 300 - page);
    assert!(rest.body["next"].is_null());
}

// A container's duration is a claim: an Ogg whose last page claims thirty seconds but
// holds one is recorded as the length its chunks measure, not the length it claims.
#[tokio::test]
async fn a_meetings_duration_is_its_measured_chunks_not_the_containers_claim() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, ogg_claiming(30)).await;
    assert_eq!(kit.work().await, Outcome::Transcoded);
    let duration = kit.show(&ana, meeting).await.body["duration_ms"]
        .as_u64()
        .unwrap();
    assert!((900..=1_100).contains(&duration), "{duration}");
}
