//! The M1 exit criterion: a thirty-minute role-played call (synthetic: two espeak-ng voices
//! reading fixtures/roleplay/call.txt, made by scripts/make-roleplay.sh) goes through the
//! whole pipeline and comes back as a speaker-labelled transcript. The recorded provider's
//! answers are built from the script's own timings, chunk by chunk, after the pinned ffmpeg
//! has cut the call: what a diarising provider returns, with the script as ground truth.
//! Skipped, with a notice, when `MEETCRM_ROLEPLAY_DIR` is unset (CI sets it).

mod common;

use axum::http::StatusCode;
use common::{Kit, answer, sha_hex};
use meetcrm_media::MediaLimits;
use meetcrm_meetings::worker::Outcome;
use serde::Deserialize;
use sqlx::Row as _;

#[derive(Deserialize)]
struct Line {
    speaker: u32,
    start_ms: u64,
    end_ms: u64,
    text: String,
}

#[tokio::test]
async fn a_thirty_minute_role_played_call_becomes_a_speaker_labelled_transcript() {
    let Some(dir) = std::env::var_os("MEETCRM_ROLEPLAY_DIR") else {
        eprintln!(
            "notice: MEETCRM_ROLEPLAY_DIR is unset; run scripts/make-roleplay.sh and point it at the output to run this test"
        );
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let audio = std::fs::read(dir.join("call.wav")).unwrap();
    let lines: Vec<Line> =
        serde_json::from_slice(&std::fs::read(dir.join("timings.json")).unwrap()).unwrap();
    let kit = Kit::tuned(|s| s.media = MediaLimits::default()).await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.uploaded(&ana, audio.clone()).await;
    let shown = kit.show(&ana, meeting).await.body;
    let duration_ms = shown["duration_ms"].as_u64().unwrap();
    assert!(
        (29 * 60_000..=31 * 60_000).contains(&duration_ms),
        "{duration_ms}"
    );
    assert_eq!(kit.work().await, Outcome::Transcoded);

    let chunks = sqlx::query(
        "SELECT idx, offset_ms, duration_ms FROM media_chunks WHERE meeting_id = $1 ORDER BY idx",
    )
    .bind(meeting)
    .fetch_all(&kit.db.pool)
    .await
    .unwrap();
    assert_eq!(chunks.len(), 3, "ten-minute chunks of a thirty-minute call");
    let recording = sha_hex(&audio);
    let mut expected = Vec::new();
    for row in &chunks {
        let idx: i32 = row.get("idx");
        let offset = u64::try_from(row.get::<i64, _>("offset_ms")).unwrap();
        let length = u64::try_from(row.get::<i64, _>("duration_ms")).unwrap();
        // A line belongs to the chunk holding its midpoint, its times clipped to the chunk.
        let mine: Vec<(u32, f64, f64, &str)> = lines
            .iter()
            .filter(|l| (offset..offset + length).contains(&u64::midpoint(l.start_ms, l.end_ms)))
            .map(|l| {
                let start = l.start_ms.max(offset) - offset;
                let end = l.end_ms.min(offset + length) - offset;
                expected.push((
                    format!("Speaker {} (part {})", l.speaker + 1, idx + 1),
                    offset + start,
                    l.text.clone(),
                ));
                (l.speaker, ms(start), ms(end), l.text.as_str())
            })
            .collect();
        kit.record(
            &recording,
            u32::try_from(idx).unwrap(),
            &answer(ms(length), &mine),
        );
    }
    assert_eq!(expected.len(), lines.len());

    let mut outcome = kit.work().await;
    for _ in 0..5 {
        if outcome != Outcome::Transcribed {
            outcome = kit.work().await;
        }
    }
    assert_eq!(outcome, Outcome::Transcribed);
    let transcript = kit.transcript(&ana, meeting).await;
    assert_eq!(transcript.status, StatusCode::OK, "{}", transcript.body);
    let got: Vec<(String, u64, String)> = transcript.body["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["speaker"].as_str().unwrap().to_owned(),
                s["start_ms"].as_u64().unwrap(),
                s["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(&expected) {
        assert_eq!(g.0, e.0);
        assert!(g.1.abs_diff(e.1) <= 1, "{g:?} vs {e:?}");
        assert_eq!(g.2, e.2);
    }
    let speakers: std::collections::BTreeSet<u32> = lines.iter().map(|l| l.speaker).collect();
    assert_eq!(speakers.len(), 2);
    let done = kit.show(&ana, meeting).await.body;
    assert_eq!(done["status"], "transcribed");
    assert_eq!(done["audio_deleted"], true, "{done}");
    kit.meetings.sweep().await.unwrap();
    assert_eq!(kit.files().await, 0);
}

fn ms(v: u64) -> f64 {
    f64::from(u32::try_from(v).unwrap_or(u32::MAX)) / 1000.0
}
