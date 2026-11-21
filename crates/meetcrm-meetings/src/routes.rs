//! The meetings routes. Every query names the workspace the caller was authorized for, so
//! another workspace's meeting is 404, never 403.

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post, put};
use http_body_util::BodyExt as _;
use meetcrm_accounts::domain::{Action, allows};
use meetcrm_accounts::error::AccountsError;
use meetcrm_accounts::{Access, Via};
use meetcrm_media::MediaError;
use meetcrm_media::store::{Incoming, Received};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
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
/// Most transcript text per page, in bytes (a page ends early past it).
pub const MAX_PAGE_TEXT_BYTES: usize = 1024 * 1024;
/// How long a committed upload's transcode job waits for its original to be placed; if
/// the process dies first, the job then finds no recording and ends the meeting.
const PLACEMENT_WINDOW: f64 = 60.0;
/// How long an upload claim outlives the upload's own deadlines.
const CLAIM_MARGIN: std::time::Duration = std::time::Duration::from_secs(60);
/// The deadline on giving a claim back.
const RELEASE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
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
fn member(access: &Access) -> Result<(Uuid, meetcrm_accounts::domain::Role), MeetingsError> {
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
        .access(&headers, workspace, Action::Record)
        .await?;
    let (user, _) = member(&access)?;
    let title = body.title.trim();
    if title.is_empty()
        || title.chars().count() > 200
        || title.chars().any(|c| c.is_control() || is_bidi_control(c))
    {
        return Err(MeetingsError::Invalid(
            "title_invalid",
            "a title is 1 to 200 characters, without control or direction-override characters",
        ));
    }
    let id = Uuid::now_v7();
    let mut tx = m.inner.db.begin().await?;
    // The meeting plan, admitted one creation at a time per workspace.
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM meetings WHERE workspace_id = $1")
        .bind(workspace)
        .fetch_one(&mut *tx)
        .await?;
    let cap = i64::from(m.inner.settings.max_meetings_per_workspace);
    let full = held >= cap; // LIMIT(meeting-plan)
    if full {
        return Err(AccountsError::PlanLimit("meetings").into());
    }
    sqlx::query(
        "INSERT INTO meetings (id, workspace_id, title, created_by) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(workspace)
    .bind(title)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "title": title, "status": "awaiting_consent" })),
    ))
}

/// Characters that reorder how text around them is shown (Trojan Source, CVE-2021-42574).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
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
        .access(&headers, workspace, Action::Record)
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

/// Receives a recording. Authorization and consent are checked, the declared length
/// compared with the cap, slots taken (server-wide, per workspace, per person) and the
/// meeting claimed against the workspace's storage plan before a single body byte is read;
/// the body is then streamed to staging under a size cap, an idle deadline and a minimum
/// rate, and probed. The row and its transcode job commit first; only then is the
/// original placed, so no path leaves a file without its record.
async fn upload(
    State(m): State<Meetings>,
    headers: HeaderMap,
    Path((workspace, meeting)): Path<(Uuid, Uuid)>,
    body: Body,
) -> Answer<(StatusCode, Json<Value>)> {
    let access = m
        .inner
        .accounts
        .access(&headers, workspace, Action::Record)
        .await?;
    let (user, _) = member(&access)?;
    // A fast refusal before any slot is taken; the claim below is what decides.
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM consents c WHERE c.meeting_id = m.id), \
                (m.status = 'receiving' AND m.receiving_until >= now()) \
                OR m.status IN ('awaiting_consent', 'transcoding', 'transcribing', 'transcribed') \
         FROM meetings m WHERE m.workspace_id = $1 AND m.id = $2",
    )
    .bind(workspace)
    .bind(meeting)
    .fetch_optional(&m.inner.db)
    .await?;
    let (consented, taken) = row.ok_or_else(MeetingsError::not_found)?;
    if !consented {
        return Err(MeetingsError::Conflict("consent_required"));
    }
    if taken {
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
    let _slots = take_slots(&m, workspace, user)?;
    let reserve = declared.unwrap_or(limits.max_upload_bytes).max(1);
    let claim = claim(&m, workspace, meeting, reserve).await?;

    let received = match receive(&m, body).await {
        Ok(received) => received,
        Err(e) => {
            claim.release().await;
            return Err(e);
        }
    };
    // A refused recording leaves nothing behind by the time the answer is sent.
    let probe = match m.inner.tools.probe(&received.path(), limits).await {
        Ok(probe) => probe,
        Err(e) => {
            received.staging.discard().await;
            claim.release().await;
            return Err(e.into());
        }
    };
    let (sha, bytes) = (received.sha256, received.bytes);
    let committed = commit(&m, &claim, &sha, bytes, probe.duration_ms).await;
    match committed {
        Ok(true) => claim.disarm(),
        Ok(false) => {
            // Deleted while it streamed, or the claim expired and another upload took it.
            received.staging.discard().await;
            claim.disarm();
            return Err(MeetingsError::Conflict("media_exists"));
        }
        Err(e) => {
            received.staging.discard().await;
            claim.release().await;
            return Err(e);
        }
    }
    let placed = m
        .inner
        .store
        .place_original(received, workspace, meeting)
        .await;
    // The job may run now: on the recording if it was placed, otherwise to find it missing
    // and end the meeting. If this update is lost, the placement window runs out instead.
    let _ = sqlx::query(
        "UPDATE jobs SET run_after = now() \
         WHERE meeting_id = $1 AND stage = 'transcode' AND state = 'queued'",
    )
    .bind(meeting)
    .execute(&m.inner.db)
    .await;
    placed?;
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

/// An upload's slots: server-wide, its workspace's and its person's; past any, 503.
fn take_slots(
    m: &Meetings,
    workspace: Uuid,
    user: Uuid,
) -> Result<(tokio::sync::OwnedSemaphorePermit, crate::Slot, crate::Slot), MeetingsError> {
    let server = m
        .inner
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| MeetingsError::Busy)?;
    let workspace = m
        .inner
        .workspace_uploads
        .try_take(workspace)
        .ok_or(MeetingsError::Busy)?;
    let user = m
        .inner
        .user_uploads
        .try_take(user)
        .ok_or(MeetingsError::Busy)?;
    Ok((server, workspace, user))
}

/// A meeting claimed by one upload. Given back (to `awaiting_media`) when the upload fails;
/// if the handler is dropped (the client went away), the drop gives it back; if the process
/// dies, the claim expires.
struct Claim {
    db: PgPool,
    meeting: Uuid,
    id: Uuid,
    armed: bool,
}

impl Claim {
    async fn release(mut self) {
        self.armed = false;
        let _ = tokio::time::timeout(
            RELEASE_DEADLINE,
            release(self.db.clone(), self.meeting, self.id),
        )
        .await;
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (db, meeting, id) = (self.db.clone(), self.meeting, self.id);
            runtime.spawn(async move {
                let _ = tokio::time::timeout(RELEASE_DEADLINE, release(db, meeting, id)).await;
            });
        }
    }
}

async fn release(db: PgPool, meeting: Uuid, claim: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meetings SET status = 'awaiting_media', upload_claim = NULL, \
         receiving_until = NULL, upload_reserved = NULL \
         WHERE id = $1 AND upload_claim = $2 AND status = 'receiving'",
    )
    .bind(meeting)
    .bind(claim)
    .execute(&db)
    .await
    .map(|_| ())
}

/// Claims `meeting` for one upload of at most `reserve` bytes: it must be consented and
/// waiting for media, or held by a claim that expired, or have failed with its audio gone
/// (a new recording is the way back from a failure). Admitted against the workspace's
/// storage plan with the workspace row locked, so two uploads cannot both squeeze in.
async fn claim(
    m: &Meetings,
    workspace: Uuid,
    meeting: Uuid,
    reserve: u64,
) -> Result<Claim, MeetingsError> {
    let s = &m.inner.settings;
    let id = Uuid::new_v4();
    let hold = s.upload_deadline + s.media.probe_deadline + CLAIM_MARGIN;
    let mut tx = m.inner.db.begin().await?;
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    let stored: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(CASE WHEN status = 'receiving' THEN upload_reserved \
                                  ELSE media_bytes END), 0)::bigint \
         FROM meetings WHERE workspace_id = $1 AND id <> $2 AND audio_deleted_at IS NULL",
    )
    .bind(workspace)
    .bind(meeting)
    .fetch_one(&mut *tx)
    .await?;
    let reserve = i64::try_from(reserve).unwrap_or(i64::MAX);
    let cap = i64::try_from(s.max_media_bytes_per_workspace).unwrap_or(i64::MAX);
    let over = stored.saturating_add(reserve) > cap; // LIMIT(media-plan)
    if over {
        return Err(AccountsError::PlanLimit("media_storage").into());
    }
    let claimed = sqlx::query(
        "UPDATE meetings SET status = 'receiving', status_reason = NULL, upload_claim = $3, \
             receiving_until = now() + make_interval(secs => $4), upload_reserved = $5, \
             media_sha256 = NULL, media_bytes = NULL, duration_ms = NULL, chunk_count = NULL, \
             audio_deleted_at = NULL, transcribed_at = NULL \
         WHERE workspace_id = $1 AND id = $2 \
           AND EXISTS (SELECT 1 FROM consents c WHERE c.meeting_id = meetings.id) \
           AND (status = 'awaiting_media' \
             OR (status = 'receiving' AND receiving_until < now()) \
             OR (status IN ('media_rejected', 'media_unavailable', 'transcription_unavailable') \
                 AND audio_deleted_at IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.meeting_id = meetings.id \
                                 AND j.state IN ('queued', 'running')) \
                 AND NOT EXISTS (SELECT 1 FROM media_garbage g \
                                 WHERE g.meeting_id = meetings.id)))",
    )
    .bind(workspace)
    .bind(meeting)
    .bind(id)
    .bind(hold.as_secs_f64())
    .bind(reserve)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Err(MeetingsError::Conflict("media_exists"));
    }
    // A meeting taking a new recording starts its pipeline again.
    sqlx::query("DELETE FROM transcript_segments WHERE meeting_id = $1")
        .bind(meeting)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM jobs WHERE meeting_id = $1")
        .bind(meeting)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM media_chunks WHERE meeting_id = $1")
        .bind(meeting)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Claim {
        db: m.inner.db.clone(),
        meeting,
        id,
        armed: true,
    })
}

/// Records the received recording and queues its transcode, behind the placement window.
/// False if the claim is no longer this upload's.
async fn commit(
    m: &Meetings,
    claim: &Claim,
    sha: &[u8; 32],
    bytes: u64,
    duration_ms: u64,
) -> Result<bool, MeetingsError> {
    let mut tx = m.inner.db.begin().await?;
    let committed = sqlx::query(
        "UPDATE meetings SET status = 'transcoding', status_reason = NULL, media_sha256 = $3, \
         media_bytes = $4, duration_ms = $5, upload_claim = NULL, receiving_until = NULL, \
         upload_reserved = NULL \
         WHERE id = $1 AND upload_claim = $2 AND status = 'receiving'",
    )
    .bind(claim.meeting)
    .bind(claim.id)
    .bind(&sha[..])
    .bind(i64::try_from(bytes).map_err(|_| MediaError::TooLarge)?)
    .bind(i64::try_from(duration_ms).map_err(|_| MediaError::TooLong)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if committed == 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO jobs (id, meeting_id, stage, run_after) \
         VALUES ($1, $2, 'transcode', now() + make_interval(secs => $3))",
    )
    .bind(Uuid::now_v7())
    .bind(claim.meeting)
    .bind(PLACEMENT_WINDOW)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Streams a body to staging: capped in size, with a deadline on every read and a minimum
/// rate overall.
async fn receive(m: &Meetings, body: Body) -> Result<Received, MeetingsError> {
    let mut incoming = m
        .inner
        .store
        .receive(m.inner.settings.media.max_upload_bytes)
        .await?;
    // A refused upload leaves nothing behind by the time the answer is sent.
    if let Err(e) = fill(m, &mut incoming, body).await {
        incoming.discard().await;
        return Err(e);
    }
    let received = incoming.finish().await?;
    if received.bytes == 0 {
        received.staging.discard().await;
        return Err(MediaError::Corrupt.into());
    }
    Ok(received)
}

/// Reads the body. Each read waits at most `upload_idle`, and after `upload_grace` the
/// body must have kept up `upload_min_bytes_per_s` on average: a body that trickles a byte
/// now and then cannot hold a slot for the whole upload deadline.
async fn fill(m: &Meetings, incoming: &mut Incoming, mut body: Body) -> Result<(), MeetingsError> {
    let s = &m.inner.settings;
    let started = tokio::time::Instant::now();
    let rate = s.upload_min_bytes_per_s.max(1);
    let mut received: u64 = 0;
    loop {
        // The moment the bytes so far stop being enough.
        let behind_at = started
            + s.upload_grace // LIMIT(upload-rate)
            + std::time::Duration::from_millis(received.saturating_mul(1000) / rate);
        let now = tokio::time::Instant::now();
        if now >= behind_at {
            return Err(MeetingsError::TooSlow);
        }
        let wait = s.upload_idle.min(behind_at - now); // LIMIT(upload-idle)
        let Ok(frame) = tokio::time::timeout(wait, body.frame()).await else {
            return Err(if tokio::time::Instant::now() >= behind_at {
                MeetingsError::TooSlow
            } else {
                MeetingsError::Stalled
            });
        };
        match frame {
            None => break,
            Some(Err(e)) if over_limit(&e) => return Err(MediaError::TooLarge.into()),
            Some(Err(_)) => return Err(MeetingsError::Stalled),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    received = received.saturating_add(data.len() as u64);
                    incoming.write(&data).await?;
                }
            }
        }
    }
    Ok(())
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
    let mut text_bytes = 0usize;
    let mut full = false;
    for row in &rows {
        let text: String = row.try_get("text")?;
        text_bytes = text_bytes.saturating_add(text.len());
        let over = text_bytes > MAX_PAGE_TEXT_BYTES; // LIMIT(transcript-page-bytes)
        if over && !segments.is_empty() {
            full = true;
            break;
        }
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
            "text": text,
        }));
        last = Some(i64::from(chunk) * CURSOR_CHUNK + i64::from(n));
    }
    let next = if full || i64::try_from(segments.len()).unwrap_or(i64::MAX) == limit {
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
        .access(&headers, workspace, Action::Record)
        .await?;
    let (user, role) = member(&access)?;
    let created_by: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT created_by FROM meetings WHERE workspace_id = $1 AND id = $2")
            .bind(workspace)
            .bind(meeting)
            .fetch_optional(&m.inner.db)
            .await?;
    let created_by = created_by.ok_or_else(MeetingsError::not_found)?;
    if created_by != Some(user) && !allows(role, Action::ManageMeetings) {
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
