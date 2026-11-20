//! The pipeline worker: claims a due job under a lease, runs its stage, and finishes it in
//! the transaction that writes the stage's output, after checking the lease is still held.

use std::time::Duration;

use meetcrm_media::tools::chunk_name;
use meetcrm_transcribe::{ChunkAudio, TranscribeError, Utterance};
use sqlx::{Postgres, Row as _, Transaction};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::Meetings;
use crate::error::MeetingsError;

/// Garbage rows removed per sweep.
const SWEEP_BATCH: i64 = 100;

/// What one turn of the worker did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was due.
    Idle,
    /// The recording became chunks; the transcribe job is queued.
    Transcoded,
    /// The recording was refused, with the reason.
    Rejected(&'static str),
    /// Every chunk is transcribed; the meeting is `transcribed` and its audio deleted.
    Transcribed,
    /// A provider or tool failure; the job waits for its next attempt.
    Retrying(&'static str),
    /// Out of attempts, or a failure no retry fixes.
    Failed(&'static str),
    /// The lease was lost (taken over, or the meeting deleted): nothing was written.
    LostLease,
}

struct Job {
    id: Uuid,
    lease: Uuid,
    meeting: Uuid,
    workspace: Uuid,
    stage: String,
    attempts: i32,
    sha: Vec<u8>,
}

/// Renews a job's lease until dropped.
struct Heartbeat(tokio::task::JoinHandle<()>);

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Meetings {
    /// Runs `settings.workers` worker loops until `shutdown`; a job in flight at shutdown
    /// is dropped and taken over when its lease runs out.
    pub async fn run_workers(self, shutdown: CancellationToken) {
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..self.inner.settings.workers.max(1) {
            let m = self.clone();
            let shutdown = shutdown.clone();
            set.spawn(async move {
                loop {
                    let outcome = tokio::select! {
                        () = shutdown.cancelled() => return,
                        outcome = m.work_once() => outcome,
                    };
                    let idle = match outcome {
                        Ok(Outcome::Idle) => true,
                        Ok(_) => false,
                        Err(e) => {
                            tracing::warn!(error = %e, "pipeline job failed");
                            true
                        }
                    };
                    if let Err(e) = m.sweep().await {
                        tracing::warn!(error = %e, "media sweep failed");
                    }
                    if idle {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep(m.inner.settings.poll) => {}
                        }
                    }
                }
            });
        }
        while set.join_next().await.is_some() {}
    }

    /// Claims one due job (or one whose lease ran out) and runs it.
    pub async fn work_once(&self) -> Result<Outcome, MeetingsError> {
        self.exhaust().await?;
        let Some(job) = self.claim().await? else {
            return Ok(Outcome::Idle);
        };
        let _heartbeat = self.heartbeat(job.id, job.lease);
        match job.stage.as_str() {
            "transcode" => self.transcode(&job).await,
            _ => self.transcribe(&job).await,
        }
    }

    /// Removes the files of deleted or transcribed meetings, then their garbage rows.
    pub async fn sweep(&self) -> Result<usize, MeetingsError> {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT workspace_id, meeting_id FROM media_garbage ORDER BY queued_at LIMIT $1",
        )
        .bind(SWEEP_BATCH)
        .fetch_all(&self.inner.db)
        .await?;
        let mut removed = 0;
        for (workspace, meeting) in rows {
            if self.sweep_one(workspace, meeting).await {
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub(crate) async fn sweep_one(&self, workspace: Uuid, meeting: Uuid) -> bool {
        if self
            .inner
            .store
            .remove_meeting(workspace, meeting)
            .await
            .is_err()
        {
            return false;
        }
        sqlx::query("DELETE FROM media_garbage WHERE meeting_id = $1")
            .bind(meeting)
            .execute(&self.inner.db)
            .await
            .is_ok()
    }

    /// A job whose lease ran out on its last attempt (a worker died on it each time) is
    /// failed rather than taken over again.
    async fn exhaust(&self) -> Result<(), MeetingsError> {
        let max = i32::try_from(self.inner.settings.max_attempts).unwrap_or(i32::MAX);
        let mut tx = self.inner.db.begin().await?;
        let gone: Vec<(Uuid, String)> = sqlx::query_as(
            "UPDATE jobs SET state = 'failed', lease = NULL, lease_until = NULL, \
             last_error = 'attempts_exhausted' \
             WHERE state = 'running' AND lease_until < now() AND attempts >= $1 \
             RETURNING meeting_id, stage",
        )
        .bind(max)
        .fetch_all(&mut *tx)
        .await?;
        for (meeting, stage) in gone {
            let status = if stage == "transcode" {
                "media_rejected"
            } else {
                "transcription_unavailable"
            };
            sqlx::query(
                "UPDATE meetings SET status = $2, status_reason = 'attempts_exhausted' WHERE id = $1",
            )
            .bind(meeting)
            .bind(status)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn claim(&self) -> Result<Option<Job>, MeetingsError> {
        let lease = Uuid::new_v4();
        let max = i32::try_from(self.inner.settings.max_attempts).unwrap_or(i32::MAX);
        let row = sqlx::query(
            "WITH next AS ( \
                 SELECT id FROM jobs \
                 WHERE (state = 'queued' AND run_after <= now()) \
                    OR (state = 'running' AND lease_until < now() AND attempts < $3) \
                 ORDER BY run_after, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             UPDATE jobs j SET state = 'running', lease = $1, \
                 lease_until = now() + make_interval(secs => $2), attempts = j.attempts + 1 \
             FROM next, meetings m \
             WHERE j.id = next.id AND m.id = j.meeting_id \
             RETURNING j.id, j.meeting_id, j.stage, j.attempts, m.workspace_id, m.media_sha256",
        )
        .bind(lease)
        .bind(self.inner.settings.lease.as_secs_f64())
        .bind(max)
        .fetch_optional(&self.inner.db)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(Job {
            id: row.try_get("id")?,
            lease,
            meeting: row.try_get("meeting_id")?,
            workspace: row.try_get("workspace_id")?,
            stage: row.try_get("stage")?,
            attempts: row.try_get("attempts")?,
            sha: row
                .try_get::<Option<Vec<u8>>, _>("media_sha256")?
                .unwrap_or_default(),
        }))
    }

    fn heartbeat(&self, job: Uuid, lease: Uuid) -> Heartbeat {
        let db = self.inner.db.clone();
        let every = self.inner.settings.lease / 3;
        let secs = self.inner.settings.lease.as_secs_f64();
        Heartbeat(tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let renewed = sqlx::query(
                    "UPDATE jobs SET lease_until = now() + make_interval(secs => $3) \
                     WHERE id = $1 AND lease = $2 AND state = 'running'",
                )
                .bind(job)
                .bind(lease)
                .bind(secs)
                .execute(&db)
                .await;
                if matches!(renewed, Ok(r) if r.rows_affected() == 0) {
                    return;
                }
            }
        }))
    }

    /// Locks the job row if this worker still holds its lease. Deleting the meeting waits
    /// on this lock, so nothing is placed for a meeting that is already gone.
    async fn hold(tx: &mut Transaction<'_, Postgres>, job: &Job) -> Result<bool, MeetingsError> {
        let held: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM jobs WHERE id = $1 AND lease = $2 AND state = 'running' FOR UPDATE",
        )
        .bind(job.id)
        .bind(job.lease)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(held.is_some())
    }

    fn backoff(&self, attempts: i32) -> Duration {
        let s = &self.inner.settings;
        let doublings = u32::try_from(attempts.saturating_sub(1))
            .unwrap_or(0)
            .min(16);
        s.retry_base.saturating_mul(1 << doublings).min(s.retry_max)
    }

    /// Ends an attempt that failed: queued again after a backoff while attempts remain and
    /// the failure is transient, otherwise failed, with the meeting showing `status`.
    async fn give_up_or_retry(
        &self,
        job: &Job,
        code: &'static str,
        transient: bool,
        status: &'static str,
    ) -> Result<Outcome, MeetingsError> {
        let retry = transient
            && u32::try_from(job.attempts).unwrap_or(u32::MAX) < self.inner.settings.max_attempts; // LIMIT(job-attempts)
        let mut tx = self.inner.db.begin().await?;
        if !Self::hold(&mut tx, job).await? {
            return Ok(Outcome::LostLease);
        }
        if retry {
            sqlx::query(
                "UPDATE jobs SET state = 'queued', lease = NULL, lease_until = NULL, \
                 run_after = now() + make_interval(secs => $2), last_error = $3 WHERE id = $1",
            )
            .bind(job.id)
            .bind(self.backoff(job.attempts).as_secs_f64())
            .bind(code)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE jobs SET state = 'failed', lease = NULL, lease_until = NULL, \
                 last_error = $2 WHERE id = $1",
            )
            .bind(job.id)
            .bind(code)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE meetings SET status = $2, status_reason = $3 WHERE id = $1")
            .bind(job.meeting)
            .bind(status)
            .bind(code)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(if retry {
            Outcome::Retrying(code)
        } else {
            Outcome::Failed(code)
        })
    }

    async fn transcode(&self, job: &Job) -> Result<Outcome, MeetingsError> {
        let store = &self.inner.store;
        let staging = store.staging().await?;
        let original = store.original(job.workspace, job.meeting);
        let chunks = match self
            .inner
            .tools
            .transcode(&original, staging.dir(), &self.inner.settings.media)
            .await
        {
            Ok(chunks) => chunks,
            Err(e) if e.is_transient() => {
                staging.discard().await;
                return self
                    .give_up_or_retry(job, e.code(), true, "transcoding")
                    .await;
            }
            Err(e) => {
                staging.discard().await;
                // Refused: nothing of the recording is kept.
                let outcome = self
                    .give_up_or_retry(job, e.code(), false, "media_rejected")
                    .await?;
                if outcome != Outcome::LostLease {
                    let _ = store.remove_meeting(job.workspace, job.meeting).await;
                    return Ok(Outcome::Rejected(e.code()));
                }
                return Ok(outcome);
            }
        };
        let mut tx = self.inner.db.begin().await?;
        if !Self::hold(&mut tx, job).await? {
            staging.discard().await;
            return Ok(Outcome::LostLease);
        }
        sqlx::query("DELETE FROM media_chunks WHERE meeting_id = $1")
            .bind(job.meeting)
            .execute(&mut *tx)
            .await?;
        let mut idx = Vec::with_capacity(chunks.len());
        let mut offsets = Vec::with_capacity(chunks.len());
        let mut durations = Vec::with_capacity(chunks.len());
        for c in &chunks {
            idx.push(i32::try_from(c.index).map_err(|_| MeetingsError::internal("chunk index"))?);
            offsets.push(
                i64::try_from(c.offset_ms).map_err(|_| MeetingsError::internal("chunk offset"))?,
            );
            durations.push(
                i64::try_from(c.duration_ms)
                    .map_err(|_| MeetingsError::internal("chunk length"))?,
            );
        }
        sqlx::query(
            "INSERT INTO media_chunks (meeting_id, idx, offset_ms, duration_ms) \
             SELECT $1, * FROM UNNEST($2::int[], $3::bigint[], $4::bigint[])",
        )
        .bind(job.meeting)
        .bind(&idx)
        .bind(&offsets)
        .bind(&durations)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE meetings SET status = 'transcribing', status_reason = NULL, chunk_count = $2 \
             WHERE id = $1",
        )
        .bind(job.meeting)
        .bind(i32::try_from(chunks.len()).map_err(|_| MeetingsError::internal("chunk count"))?)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE jobs SET state = 'done', lease = NULL, lease_until = NULL WHERE id = $1",
        )
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO jobs (id, meeting_id, stage) VALUES ($1, $2, 'transcribe') \
             ON CONFLICT (meeting_id, stage) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(job.meeting)
        .execute(&mut *tx)
        .await?;
        store
            .place_chunks(staging, job.workspace, job.meeting)
            .await?;
        tx.commit().await?;
        Ok(Outcome::Transcoded)
    }

    async fn transcribe(&self, job: &Job) -> Result<Outcome, MeetingsError> {
        let pending: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT idx, duration_ms FROM media_chunks \
             WHERE meeting_id = $1 AND NOT transcribed ORDER BY idx",
        )
        .bind(job.meeting)
        .fetch_all(&self.inner.db)
        .await?;
        let recording = hex::encode(&job.sha);
        let dir = self.inner.store.chunks(job.workspace, job.meeting);
        for (idx, duration_ms) in pending {
            let index = u32::try_from(idx).map_err(|_| MeetingsError::internal("chunk index"))?;
            let path = dir.join(chunk_name(index));
            let audio = ChunkAudio {
                path: &path,
                duration_ms: u64::try_from(duration_ms).unwrap_or(0),
                recording: &recording,
                index,
            };
            let answer = tokio::time::timeout(
                self.inner.settings.provider_deadline, // LIMIT(job-provider-deadline)
                self.inner.transcriber.transcribe(audio),
            )
            .await
            .unwrap_or(Err(TranscribeError::Timeout));
            match answer {
                Ok(utterances) => {
                    if !self.apply(job, idx, &utterances).await? {
                        return Ok(Outcome::LostLease);
                    }
                }
                Err(e) => {
                    // A partial answer is never stored: the chunk stays untranscribed.
                    return self
                        .give_up_or_retry(
                            job,
                            e.code(),
                            e.is_transient(),
                            "transcription_unavailable",
                        )
                        .await;
                }
            }
        }
        let mut tx = self.inner.db.begin().await?;
        if !Self::hold(&mut tx, job).await? {
            return Ok(Outcome::LostLease);
        }
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM media_chunks WHERE meeting_id = $1 AND NOT transcribed",
        )
        .bind(job.meeting)
        .fetch_one(&mut *tx)
        .await?;
        if left != 0 {
            return Err(MeetingsError::internal("chunks left untranscribed"));
        }
        sqlx::query(
            "UPDATE meetings SET status = 'transcribed', status_reason = NULL, \
             transcribed_at = now(), audio_deleted_at = now() WHERE id = $1",
        )
        .bind(job.meeting)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE jobs SET state = 'done', lease = NULL, lease_until = NULL WHERE id = $1",
        )
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        // Audio is deleted once transcribed: queued here, removed by the sweep.
        sqlx::query(
            "INSERT INTO media_garbage (meeting_id, workspace_id) VALUES ($1, $2) \
             ON CONFLICT (meeting_id) DO NOTHING",
        )
        .bind(job.meeting)
        .bind(job.workspace)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.sweep_one(job.workspace, job.meeting).await;
        Ok(Outcome::Transcribed)
    }

    /// Stores one chunk's transcript, once: the chunk's flag flips in the same transaction,
    /// so a retry that answers again for a stored chunk writes nothing. False if the lease
    /// is gone.
    async fn apply(
        &self,
        job: &Job,
        idx: i32,
        utterances: &[Utterance],
    ) -> Result<bool, MeetingsError> {
        let mut tx = self.inner.db.begin().await?;
        if !Self::hold(&mut tx, job).await? {
            return Ok(false);
        }
        let flipped = sqlx::query(
            "UPDATE media_chunks SET transcribed = true \
             WHERE meeting_id = $1 AND idx = $2 AND NOT transcribed",
        )
        .bind(job.meeting)
        .bind(idx)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if flipped == 0 {
            return Ok(true);
        }
        let mut n = Vec::with_capacity(utterances.len());
        let mut speaker = Vec::with_capacity(utterances.len());
        let mut start = Vec::with_capacity(utterances.len());
        let mut end = Vec::with_capacity(utterances.len());
        let mut text = Vec::with_capacity(utterances.len());
        for u in utterances.iter().filter(|u| !u.text.is_empty()) {
            n.push(i32::try_from(n.len()).unwrap_or(i32::MAX));
            speaker.push(i32::try_from(u.speaker).unwrap_or(i32::MAX));
            start.push(i64::try_from(u.start_ms).unwrap_or(i64::MAX));
            end.push(i64::try_from(u.end_ms).unwrap_or(i64::MAX));
            text.push(u.text.as_str());
        }
        sqlx::query(
            "INSERT INTO transcript_segments (meeting_id, chunk, n, speaker, start_ms, end_ms, text) \
             SELECT $1, $2, * FROM UNNEST($3::int[], $4::int[], $5::bigint[], $6::bigint[], $7::text[])",
        )
        .bind(job.meeting)
        .bind(idx)
        .bind(&n)
        .bind(&speaker)
        .bind(&start)
        .bind(&end)
        .bind(&text)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
