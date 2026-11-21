//! The per-workspace plan limits on meetings and stored recordings, checked at admission.

mod common;

use axum::http::{Method, StatusCode};
use common::{Kit, answer, sha_hex, tone};
use meetcrm_meetings::worker::Outcome;

async fn consented(kit: &Kit, who: &common::Person) -> uuid::Uuid {
    let meeting = kit.meeting(who).await;
    kit.consent(who, meeting).await;
    meeting
}

#[tokio::test]
async fn a_workspace_past_its_storage_plan_is_refused_at_admission() {
    let audio = tone(1);
    let size = u64::try_from(audio.len()).unwrap();
    let kit = Kit::tuned(|s| s.max_media_bytes_per_workspace = size * 3 / 2).await;
    let ana = kit.person("ana@example.test").await;
    kit.record(
        &sha_hex(&audio),
        0,
        &answer(1.0, &[(0, 0.0, 0.5, "Hello.")]),
    );
    let first = kit.uploaded(&ana, audio.clone()).await;
    let second = consented(&kit, &ana).await;
    let refused = kit.upload(&ana, second, audio.clone()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code(), Some("plan_limit"));
    assert_eq!(refused.body["error"]["limit"], "media_storage");
    assert_eq!(
        kit.show(&ana, second).await.body["status"],
        "awaiting_media"
    );
    // Once the first recording's audio is deleted, its bytes no longer count.
    assert_eq!(kit.work().await, Outcome::Transcoded);
    assert_eq!(kit.work().await, Outcome::Transcribed);
    assert_eq!(kit.show(&ana, first).await.body["audio_deleted"], true);
    let accepted = kit.upload(&ana, second, audio).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.body);
}

#[tokio::test]
async fn a_workspace_past_its_meeting_plan_is_refused() {
    let kit = Kit::tuned(|s| s.max_meetings_per_workspace = 2).await;
    let ana = kit.person("ana@example.test").await;
    kit.meeting(&ana).await;
    kit.meeting(&ana).await;
    let refused = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{}/meetings", ana.workspace),
            Some(&ana.session),
            Some(serde_json::json!({ "title": "Third" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.body["error"]["limit"], "meetings");
    let ben = kit.person("ben@example.test").await;
    kit.meeting(&ben).await;
}
