//! The meetings routes. Every query names the workspace the caller was authorized for, so
//! another workspace's meeting is 404, never 403.

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post, put};
use http_body_util::BodyExt as _;
use meetcrm_accounts::domain::{Action, Role};
use meetcrm_accounts::error::AccountsError;
use meetcrm_accounts::{Access, Via};
use meetcrm_media::MediaError;
use meetcrm_media::store::Received;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::error::MeetingsError;
use crate::{Meetings, STATEMENTS};

/// Most meetings per page.
pub const MAX_PAGE: i64 = 100;
/// Most transcript segments per page.
pub const MAX_SEGMENTS: i64 = 1000;
/// A transcript cursor is `chunk * CURSOR_CHUNK + n`.
const CURSOR_CHUNK: i64 = 1_000_000;

type Answer<T = Json<Value>> = Result<T, MeetingsError>;

/// The routes under the global body and time limits.
pub fn router(meetings: Meetings) -> Router {
    Router::new()
        .route("/workspaces/{workspace}/meetings", post(create).get(list))
        .route(
            "/workspaces/{workspace}/meetings/{meeting}",
            get(show).delete(remove),
        )
        .route(
            "/workspaces/{workspace}/meetings/{meeting}/consent",
            post(consent),
        )
        .route(
            "/workspaces/{workspace}/meetings/{meeting}/transcript",
            get(transcript),
        )
        .with_state(meetings)
}

/// The upload route, which the server mounts under its own body and time limits.
pub fn upload_router(meetings: Meetings) -> Router {
    Router::new()
        .route(
            "/workspaces/{workspace}/meetings/{meeting}/media",
            put(upload),
        )
        .with_state(meetings)
}

/// Writes need a signed-in member; returns who.
fn member(access: &Access) -> Result<(Uuid, Role), MeetingsError> {
    match access.via {
        Via::Member { user, role } => Ok((user, role)),
        Via::Token { .. } => Err(AccountsError::SessionRequired.into()),
    }
}

fn time(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMeeting {
    title: String,
}

async fn create(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
    Json(body): Json<NewMeeting>,
) -> Answer<(StatusCode, Json<Value>)> {
    let access = m
        .inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let (user, _) = member(&access)?;
    let title = body.title.trim();
    if title.is_empty() || title.chars().count() > 200 || title.chars().any(char::is_control) {
        return Err(MeetingsError::Invalid(
            "title_invalid",
            "a title is 1 to 200 characters, without control characters",
        ));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO meetings (id, workspace_id, title, created_by) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(workspace)
    .bind(title)
    .bind(user)
    .execute(&m.inner.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "title": title, "status": "awaiting_consent" })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    before: Option<Uuid>,
    limit: Option<i64>,
}

macro_rules! meeting_columns {
    () => {
        "m.id, m.title, m.status, m.status_reason, m.created_at, m.duration_ms, m.media_bytes, \
         m.chunk_count, m.audio_deleted_at, m.transcribed_at, \
         EXISTS (SELECT 1 FROM consents c WHERE c.meeting_id = m.id) AS consented"
    };
}

fn meeting_json(row: &sqlx::postgres::PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "id": row.try_get::<Uuid, _>("id")?,
        "title": row.try_get::<String, _>("title")?,
        "status": row.try_get::<String, _>("status")?,
        "reason": row.try_get::<Option<String>, _>("status_reason")?,
        "created_at": time(row.try_get("created_at")?),
        "consent": row.try_get::<bool, _>("consented")?,
        "duration_ms": row.try_get::<Option<i64>, _>("duration_ms")?,
        "media_bytes": row.try_get::<Option<i64>, _>("media_bytes")?,
        "chunks": row.try_get::<Option<i32>, _>("chunk_count")?,
        "audio_deleted": row.try_get::<Option<OffsetDateTime>, _>("audio_deleted_at")?.is_some(),
        "transcribed_at": row.try_get::<Option<OffsetDateTime>, _>("transcribed_at")?.map(time),
    }))
}

async fn list(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
    Query(page): Query<Page>,
) -> Answer {
    m.inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let limit = page.limit.unwrap_or(50).clamp(1, MAX_PAGE);
    let rows = sqlx::query(concat!(
        "SELECT ",
        meeting_columns!(),
        " FROM meetings m WHERE m.workspace_id = $1 AND ($2::uuid IS NULL OR m.id < $2) \
         ORDER BY m.id DESC LIMIT $3"
    ))
    .bind(workspace)
    .bind(page.before)
    .bind(limit)
    .fetch_all(&m.inner.db)
    .await?;
    let meetings = rows
        .iter()
        .map(meeting_json)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if i64::try_from(meetings.len()).unwrap_or(i64::MAX) == limit {
        meetings.last().map(|v| v["id"].clone())
    } else {
        None
    };
    Ok(Json(json!({ "meetings": meetings, "next": next })))
}

async fn show(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
) -> Answer {
    m.inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let row = sqlx::query(concat!(
        "SELECT ",
        meeting_columns!(),
        " FROM meetings m WHERE m.workspace_id = $1 AND m.id = $2"
    ))
    .bind(workspace)
    .bind(meeting)
    .fetch_optional(&m.inner.db)
    .await?
    .ok_or_else(MeetingsError::not_found)?;
    Ok(Json(meeting_json(&row)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Consent {
    statement: String,
    participants: i32,
}

async fn consent(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
    Json(body): Json<Consent>,
) -> Answer<(StatusCode, Json<Value>)> {
    let access = m
        .inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let (user, _) = member(&access)?;
    if !STATEMENTS.contains(&body.statement.as_str()) {
        return Err(MeetingsError::Invalid(
            "statement_invalid",
            "the statement is not one we accept",
        ));
    }
    if !(2..=100).contains(&body.participants) {
        return Err(MeetingsError::Invalid(
            "participants_invalid",
            "participants is 2 to 100",
        ));
    }
    let mut tx = m.inner.db.begin().await?;
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM meetings WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace)
    .bind(meeting)
    .fetch_optional(&mut *tx)
    .await?;
    match status.as_deref() {
        None => return Err(MeetingsError::not_found()),
        Some("awaiting_consent") => {}
        Some(_) => return Err(MeetingsError::Conflict("consent_exists")),
    }
    sqlx::query(
        "INSERT INTO consents (meeting_id, workspace_id, attested_by, statement, participants) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(meeting)
    .bind(workspace)
    .bind(user)
    .bind(&body.statement)
    .bind(body.participants)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE meetings SET status = 'awaiting_media' WHERE id = $1")
        .bind(meeting)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "meeting": meeting, "statement": body.statement, "status": "awaiting_media" }),
        ),
    ))
}

/// Receives a recording. Authorization and consent are checked, and the declared length
/// compared with the cap, before a single body byte is read; the body is then streamed to
/// staging under a size cap and an idle deadline, probed, and only then placed under the
/// meeting with its transcode job, in one transaction.
async fn upload(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
    body: Body,
) -> Answer<(StatusCode, Json<Value>)> {
    let access = m
        .inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    member(&access)?;
    let row = sqlx::query(
        "SELECT m.status, EXISTS (SELECT 1 FROM consents c WHERE c.meeting_id = m.id) AS consented \
         FROM meetings m WHERE m.workspace_id = $1 AND m.id = $2",
    )
    .bind(workspace)
    .bind(meeting)
    .fetch_optional(&m.inner.db)
    .await?
    .ok_or_else(MeetingsError::not_found)?;
    if !row.try_get::<bool, _>("consented")? {
        return Err(MeetingsError::Conflict("consent_required"));
    }
    if row.try_get::<String, _>("status")? != "awaiting_media" {
        return Err(MeetingsError::Conflict("media_exists"));
    }
    let limits = &m.inner.settings.media;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limits.max_upload_bytes) {
        return Err(MediaError::TooLarge.into());
    }
    let _slot = m
        .inner
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| MeetingsError::Busy)?;

    let received = receive(&m, body).await?;
    let probe = m.inner.tools.probe(&received.path(), limits).await?;

    let (sha, bytes) = (received.sha256, received.bytes);
    let mut tx = m.inner.db.begin().await?;
    let claimed = sqlx::query(
        "UPDATE meetings SET status = 'transcoding', status_reason = NULL, media_sha256 = $3, \
         media_bytes = $4, duration_ms = $5 \
         WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_media'",
    )
    .bind(workspace)
    .bind(meeting)
    .bind(&sha[..])
    .bind(i64::try_from(bytes).map_err(|_| MediaError::TooLarge)?)
    .bind(i64::try_from(probe.duration_ms).map_err(|_| MediaError::TooLong)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        // Deleted, or another upload won while this one streamed.
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM meetings WHERE workspace_id = $1 AND id = $2")
                .bind(workspace)
                .bind(meeting)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(match exists {
            Some(_) => MeetingsError::Conflict("media_exists"),
            None => MeetingsError::not_found(),
        });
    }
    sqlx::query("INSERT INTO jobs (id, meeting_id, stage) VALUES ($1, $2, 'transcode')")
        .bind(Uuid::now_v7())
        .bind(meeting)
        .execute(&mut *tx)
        .await?;
    m.inner
        .store
        .place_original(received, workspace, meeting)
        .await?;
    if let Err(e) = tx.commit().await {
        let _ = m.inner.store.remove_meeting(workspace, meeting).await;
        return Err(e.into());
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "meeting": meeting,
            "status": "transcoding",
            "media_bytes": bytes,
            "duration_ms": probe.duration_ms,
            "sha256": hex::encode(sha),
        })),
    ))
}

/// Streams a body to staging: capped in size, with a deadline on every read.
async fn receive(m: &Meetings, mut body: Body) -> Result<Received, MeetingsError> {
    let mut incoming = m
        .inner
        .store
        .receive(m.inner.settings.media.max_upload_bytes)
        .await?;
    loop {
        let frame = tokio::time::timeout(m.inner.settings.upload_idle, body.frame())
            .await
            .map_err(|_| MeetingsError::Stalled)?;
        match frame {
            None => break,
            Some(Err(e)) if over_limit(&e) => return Err(MediaError::TooLarge.into()),
            Some(Err(_)) => return Err(MeetingsError::Stalled),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    incoming.write(&data).await?;
                }
            }
        }
    }
    let received = incoming.finish().await?;
    if received.bytes == 0 {
        return Err(MediaError::Corrupt.into());
    }
    Ok(received)
}

/// Whether a body error is the stack's body limit.
fn over_limit(e: &axum::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        if c.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        cause = c.source();
    }
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentPage {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn transcript(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
    Query(page): Query<SegmentPage>,
) -> Answer {
    m.inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let row =
        sqlx::query("SELECT status, chunk_count FROM meetings WHERE workspace_id = $1 AND id = $2")
            .bind(workspace)
            .bind(meeting)
            .fetch_optional(&m.inner.db)
            .await?
            .ok_or_else(MeetingsError::not_found)?;
    if row.try_get::<String, _>("status")? != "transcribed" {
        return Err(MeetingsError::Conflict("transcript_incomplete"));
    }
    let parts = row.try_get::<Option<i32>, _>("chunk_count")?.unwrap_or(1);
    let limit = page.limit.unwrap_or(500).clamp(1, MAX_SEGMENTS);
    let after = page.after.unwrap_or(-1);
    let (after_chunk, after_n) = if after < 0 {
        (-1, -1)
    } else {
        (after / CURSOR_CHUNK, after % CURSOR_CHUNK)
    };
    let rows = sqlx::query(
        "SELECT s.chunk, s.n, s.speaker, c.offset_ms + s.start_ms AS start_ms, \
                c.offset_ms + s.end_ms AS end_ms, s.text \
         FROM transcript_segments s \
         JOIN media_chunks c ON c.meeting_id = s.meeting_id AND c.idx = s.chunk \
         WHERE s.meeting_id = $1 AND (s.chunk, s.n) > ($2::int, $3::int) \
         ORDER BY s.chunk, s.n LIMIT $4",
    )
    .bind(meeting)
    .bind(i32::try_from(after_chunk).unwrap_or(i32::MAX))
    .bind(i32::try_from(after_n).unwrap_or(i32::MAX))
    .bind(limit)
    .fetch_all(&m.inner.db)
    .await?;
    let mut segments = Vec::with_capacity(rows.len());
    let mut last = None;
    for row in &rows {
        let chunk: i32 = row.try_get("chunk")?;
        let n: i32 = row.try_get("n")?;
        let speaker: i32 = row.try_get("speaker")?;
        let label = if parts > 1 {
            format!("Speaker {} (part {})", speaker + 1, chunk + 1)
        } else {
            format!("Speaker {}", speaker + 1)
        };
        segments.push(json!({
            "speaker": label,
            "start_ms": row.try_get::<i64, _>("start_ms")?,
            "end_ms": row.try_get::<i64, _>("end_ms")?,
            "text": row.try_get::<String, _>("text")?,
        }));
        last = Some(i64::from(chunk) * CURSOR_CHUNK + i64::from(n));
    }
    let next = if i64::try_from(segments.len()).unwrap_or(i64::MAX) == limit {
        last
    } else {
        None
    };
    Ok(Json(
        json!({ "meeting": meeting, "segments": segments, "next": next }),
    ))
}

/// Deletes a meeting: its rows go in one statement (consent, chunks, transcript and jobs
/// by cascade), the trigger queues its files, and the files are removed before answering;
/// if removal fails, the worker's sweep finishes it.
async fn remove(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
) -> Answer<StatusCode> {
    let access = m
        .inner
        .accounts
        .access(&headers, workspace, Action::Read)
        .await?;
    let (user, role) = member(&access)?;
    let created_by: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT created_by FROM meetings WHERE workspace_id = $1 AND id = $2")
            .bind(workspace)
            .bind(meeting)
            .fetch_optional(&m.inner.db)
            .await?;
    let created_by = created_by.ok_or_else(MeetingsError::not_found)?;
    if created_by != Some(user) && !matches!(role, Role::Owner | Role::Admin) {
        return Err(AccountsError::Forbidden.into());
    }
    let gone = sqlx::query("DELETE FROM meetings WHERE workspace_id = $1 AND id = $2")
        .bind(workspace)
        .bind(meeting)
        .execute(&m.inner.db)
        .await?
        .rows_affected();
    if gone == 0 {
        return Err(MeetingsError::not_found());
    }
    m.sweep_one(workspace, meeting).await;
    Ok(StatusCode::NO_CONTENT)
}
