//! Route-level tenant isolation for every meetings endpoint, and who may write: another
//! workspace's meeting is not found through any route, a member of one workspace cannot
//! reach another's, and an API token reads but never writes.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::Kit;
use serde_json::json;

/// Every meetings route, as (method, path under the meeting, JSON body).
fn routes() -> Vec<(Method, &'static str, Option<serde_json::Value>)> {
    vec![
        (Method::GET, "", None),
        (Method::DELETE, "", None),
        (Method::GET, "/transcript", None),
        (
            Method::POST,
            "/consent",
            Some(json!({ "statement": "all_participants_consented", "participants": 2 })),
        ),
    ]
}

#[tokio::test]
async fn another_workspaces_meeting_is_not_found_through_any_route() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let bo = kit.person("bo@example.test").await;
    let meeting = kit.uploaded(&ana, common::tone(4)).await;

    // Bo, in his own workspace, naming Ana's meeting.
    for (method, tail, body) in routes() {
        let path = format!("/v1/workspaces/{}/meetings/{meeting}{tail}", bo.workspace);
        let answer = kit
            .call(method.clone(), &path, Some(&bo.session), body)
            .await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method} {tail}: {}",
            answer.body
        );
    }
    let upload = kit.upload(&bo, meeting, common::tone(1)).await;
    assert_eq!(upload.status, StatusCode::NOT_FOUND, "{}", upload.body);
    let listed = kit
        .call(
            Method::GET,
            &format!("/v1/workspaces/{}/meetings", bo.workspace),
            Some(&bo.session),
            None,
        )
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(
        listed.body["meetings"].as_array().unwrap().len(),
        0,
        "{}",
        listed.body
    );

    // Bo naming Ana's workspace: he is not a member, so nothing there exists for him.
    for (method, tail, body) in routes() {
        let path = format!("/v1/workspaces/{}/meetings/{meeting}{tail}", ana.workspace);
        let answer = kit
            .call(method.clone(), &path, Some(&bo.session), body)
            .await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method} {tail}: {}",
            answer.body
        );
    }
    let foreign = common::Person {
        user: bo.user,
        session: bo.session.clone(),
        workspace: ana.workspace,
    };
    let upload = kit.upload(&foreign, meeting, common::tone(1)).await;
    assert_eq!(upload.status, StatusCode::NOT_FOUND, "{}", upload.body);
    let created = kit
        .call(
            Method::POST,
            &format!("/v1/workspaces/{}/meetings", ana.workspace),
            Some(&bo.session),
            Some(json!({ "title": "Planted" })),
        )
        .await;
    assert_eq!(created.status, StatusCode::NOT_FOUND, "{}", created.body);

    // Ana's meeting is untouched.
    let shown = kit.show(&ana, meeting).await;
    assert_eq!(shown.status, StatusCode::OK);
    assert_eq!(shown.body["status"], "transcoding", "{}", shown.body);
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM consents WHERE meeting_id = $1",
            meeting
        )
        .await,
        1
    );
}

#[tokio::test]
async fn an_api_token_reads_meetings_but_never_writes() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    let token = kit.token(&ana).await;
    let base = format!("/v1/workspaces/{}/meetings", ana.workspace);

    let listed = kit.token_call(Method::GET, &base, &token).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let shown = kit
        .token_call(Method::GET, &format!("{base}/{meeting}"), &token)
        .await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.body);

    let consent = json!({ "statement": "all_participants_consented", "participants": 2 });
    for (method, path, body) in [
        (
            Method::POST,
            base.clone(),
            json!({ "title": "Planted" }).to_string(),
        ),
        (
            Method::POST,
            format!("{base}/{meeting}/consent"),
            consent.to_string(),
        ),
        (
            Method::PUT,
            format!("{base}/{meeting}/media"),
            String::from("RIFF"),
        ),
        (Method::DELETE, format!("{base}/{meeting}"), String::new()),
    ] {
        let request = Request::builder()
            .method(method.clone())
            .uri(&path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let answer = kit.send(request).await;
        assert!(
            answer.status == StatusCode::UNAUTHORIZED || answer.status == StatusCode::FORBIDDEN,
            "{method} {path}: {} {}",
            answer.status,
            answer.body
        );
    }
    assert_eq!(
        kit.count("SELECT count(*) FROM meetings WHERE id = $1", meeting)
            .await,
        1
    );
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM consents WHERE meeting_id = $1",
            meeting
        )
        .await,
        0
    );
}

#[tokio::test]
async fn consent_must_name_its_statement_and_participants() {
    let kit = Kit::new().await;
    let ana = kit.person("ana@example.test").await;
    let meeting = kit.meeting(&ana).await;
    let path = format!(
        "/v1/workspaces/{}/meetings/{meeting}/consent",
        ana.workspace
    );
    for body in [
        json!({ "statement": "maybe", "participants": 2 }),
        json!({ "statement": "all_participants_consented", "participants": 0 }),
        json!({ "statement": "all_participants_consented", "participants": 100_000 }),
        json!({ "statement": "all_participants_consented" }),
        json!({ "statement": "all_participants_consented", "participants": 2, "extra": 1 }),
    ] {
        let answer = kit
            .call(Method::POST, &path, Some(&ana.session), Some(body.clone()))
            .await;
        assert!(
            answer.status.is_client_error(),
            "{body}: {} {}",
            answer.status,
            answer.body
        );
    }
    assert_eq!(
        kit.count(
            "SELECT count(*) FROM consents WHERE meeting_id = $1",
            meeting
        )
        .await,
        0
    );
    let refused = kit.upload(&ana, meeting, common::tone(1)).await;
    assert_eq!(refused.code(), Some("consent_required"), "{}", refused.body);
}
