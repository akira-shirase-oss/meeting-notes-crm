-- 0003: meetings, consent, recordings and transcripts (M1, crates/meetcrm-meetings).
--
-- A meeting belongs to one workspace; every query names the workspace. Consent is a row
-- written once, before any upload is read. The recording itself is a file
-- (crates/meetcrm-media); the database holds its digest, size and duration, the chunks it
-- was cut into, and the transcript segments. Jobs are the pipeline's queue: one row per
-- meeting and stage, claimed under a lease, so a crashed worker's job is taken over and a
-- stage's output is written in the same transaction that finishes it. Deleting a meeting
-- (or its workspace, by cascade) queues its files for removal in media_garbage by
-- trigger, so no path deletes rows and forgets the files.

CREATE TABLE meetings (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    title text NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    created_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    status text NOT NULL DEFAULT 'awaiting_consent' CHECK (status IN (
        'awaiting_consent', 'awaiting_media', 'transcoding', 'transcribing',
        'transcribed', 'transcription_unavailable', 'media_rejected')),
    status_reason text CHECK (status_reason ~ '^[a-z_]{1,64}$'),
    media_sha256 bytea CHECK (length(media_sha256) = 32),
    media_bytes bigint CHECK (media_bytes > 0),
    duration_ms bigint CHECK (duration_ms > 0),
    chunk_count integer CHECK (chunk_count BETWEEN 1 AND 1000),
    audio_deleted_at timestamptz,
    transcribed_at timestamptz
);
CREATE INDEX meetings_workspace ON meetings (workspace_id, id DESC);

CREATE TABLE consents (
    meeting_id uuid PRIMARY KEY REFERENCES meetings (id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    attested_by uuid REFERENCES users (id) ON DELETE SET NULL,
    statement text NOT NULL CHECK (statement ~ '^[a-z0-9_]{1,64}$'),
    participants integer NOT NULL CHECK (participants BETWEEN 2 AND 100),
    attested_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE media_chunks (
    meeting_id uuid NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
    idx integer NOT NULL CHECK (idx BETWEEN 0 AND 999),
    offset_ms bigint NOT NULL CHECK (offset_ms >= 0),
    duration_ms bigint NOT NULL CHECK (duration_ms > 0),
    transcribed boolean NOT NULL DEFAULT false,
    PRIMARY KEY (meeting_id, idx)
);

CREATE TABLE transcript_segments (
    meeting_id uuid NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
    chunk integer NOT NULL,
    n integer NOT NULL CHECK (n >= 0),
    speaker integer NOT NULL CHECK (speaker BETWEEN 0 AND 31),
    start_ms bigint NOT NULL CHECK (start_ms >= 0),
    end_ms bigint NOT NULL CHECK (end_ms >= start_ms),
    text text NOT NULL CHECK (length(text) BETWEEN 1 AND 4000),
    PRIMARY KEY (meeting_id, chunk, n),
    FOREIGN KEY (meeting_id, chunk) REFERENCES media_chunks (meeting_id, idx) ON DELETE CASCADE
);

CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    meeting_id uuid NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
    stage text NOT NULL CHECK (stage IN ('transcode', 'transcribe')),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'done', 'failed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    run_after timestamptz NOT NULL DEFAULT now(),
    lease uuid,
    lease_until timestamptz,
    last_error text CHECK (last_error ~ '^[a-z_]{1,64}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (meeting_id, stage)
);
CREATE INDEX jobs_due ON jobs (run_after) WHERE state = 'queued';
CREATE INDEX jobs_leased ON jobs (lease_until) WHERE state = 'running';

-- Files to remove: no foreign key, so the row outlives the meeting it names.
CREATE TABLE media_garbage (
    meeting_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    queued_at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION queue_media_garbage() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    INSERT INTO public.media_garbage (meeting_id, workspace_id)
    VALUES (OLD.id, OLD.workspace_id)
    ON CONFLICT (meeting_id) DO NOTHING;
    RETURN OLD;
END
$$;

CREATE TRIGGER meetings_queue_garbage AFTER DELETE ON meetings
    FOR EACH ROW EXECUTE FUNCTION queue_media_garbage();
ALTER TABLE meetings ENABLE ALWAYS TRIGGER meetings_queue_garbage;

GRANT SELECT, INSERT, UPDATE, DELETE ON meetings, media_chunks, transcript_segments, jobs, media_garbage
    TO meetcrm_app;
-- Consent is evidence: written once, never changed (it goes with its meeting).
GRANT SELECT, INSERT, DELETE ON consents TO meetcrm_app;
