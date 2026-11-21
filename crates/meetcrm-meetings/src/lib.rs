//! Meetings: a meeting, its consent attestation, the consent-gated upload, the transcode
//! and transcribe jobs, the speaker-labelled transcript and deletion.
//!
//! The pipeline is a table of jobs (`jobs`, one per meeting and stage) claimed under a
//! lease, so a crashed worker's job is taken over when its lease runs out. Each stage
//! writes its output in the same transaction that finishes it, after checking it still
//! holds the lease; a transcribed chunk is marked in that transaction, so a retried job
//! never applies a chunk twice. Files follow the rows: an original is placed only after the
//! row that names it commits (its transcode job waits out a placement window), and files
//! are removed through `media_garbage`, which a trigger fills whenever a meeting row goes
//! and every terminal failure fills too, so no failed or deleted meeting keeps its audio;
//! `audio_deleted_at` is set only once the removal succeeded.
//!
//! An upload claims its meeting (`receiving`, under a claim that expires) before a body
//! byte is read, takes a server-wide, a per-workspace and a per-person slot, must keep a
//! minimum rate, and is admitted against the workspace's stored-bytes plan.

pub mod error;
pub mod routes;
pub mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meetcrm_accounts::Accounts;
use meetcrm_media::MediaLimits;
use meetcrm_media::store::FileStore;
use meetcrm_media::tools::Tools;
use meetcrm_transcribe::Transcriber;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

pub use routes::{router, upload_router};

/// The consent statements a person may attest to. One today: everyone on the call was told
/// it is recorded and agreed.
pub const STATEMENTS: &[&str] = &["all_participants_consented"];

/// Tunables; the server reads them from its configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Size, duration, CPU, output and chunk caps for the media tools.
    pub media: MediaLimits,
    /// Longest wait for the next piece of an upload body.
    pub upload_idle: Duration,
    /// Uploads received at once; past it, 503 `uploads_busy`.
    pub uploads_at_once: usize,
    /// Uploads one workspace may have in progress at once.
    pub uploads_per_workspace: usize,
    /// Uploads one person may have in progress at once.
    pub uploads_per_user: usize,
    /// The slowest an upload may arrive, after `upload_grace`, in bytes per second.
    pub upload_min_bytes_per_s: u64,
    /// How long an upload may run before it must keep up the minimum rate.
    pub upload_grace: Duration,
    /// The whole upload's deadline (the server's upload stack enforces it); a claim on a
    /// meeting outlives it by the probe deadline and a margin, then expires.
    pub upload_deadline: Duration,
    /// Meetings one workspace may hold.
    pub max_meetings_per_workspace: u32,
    /// Bytes of recordings one workspace may store (originals not yet deleted, and the
    /// uploads in progress at their declared or largest size).
    pub max_media_bytes_per_workspace: u64,
    /// Deadline for one chunk's transcription, on top of the adapter's own.
    pub provider_deadline: Duration,
    /// Attempts per job, the first included.
    pub max_attempts: u32,
    /// How long a claimed job is held without a heartbeat.
    pub lease: Duration,
    /// The first retry's delay; doubled per attempt up to `retry_max`.
    pub retry_base: Duration,
    /// The longest retry delay.
    pub retry_max: Duration,
    /// How often an idle worker looks for work.
    pub poll: Duration,
    /// Jobs worked at once by one process.
    pub workers: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            media: MediaLimits::default(),
            upload_idle: Duration::from_secs(30),
            uploads_at_once: 4,
            uploads_per_workspace: 2,
            uploads_per_user: 1,
            upload_min_bytes_per_s: 16 * 1024,
            upload_grace: Duration::from_secs(10),
            upload_deadline: Duration::from_mins(30),
            max_meetings_per_workspace: 10_000,
            max_media_bytes_per_workspace: 2 * 1024 * 1024 * 1024,
            provider_deadline: Duration::from_mins(15),
            max_attempts: 5,
            lease: Duration::from_secs(60),
            retry_base: Duration::from_secs(30),
            retry_max: Duration::from_mins(15),
            poll: Duration::from_secs(1),
            workers: 2,
        }
    }
}

/// The meetings service: cheap to clone.
#[derive(Clone)]
pub struct Meetings {
    inner: Arc<Inner>,
}

struct Inner {
    db: PgPool,
    accounts: Accounts,
    store: FileStore,
    tools: Tools,
    transcriber: Arc<dyn Transcriber>,
    settings: Settings,
    uploads: Arc<Semaphore>,
    workspace_uploads: Arc<Slots>,
    user_uploads: Arc<Slots>,
}

/// Counted slots per key (a workspace, a person). Keys are present only while a slot is
/// held, and a slot is taken only under a server-wide permit, so the map stays as small as
/// the server-wide cap.
pub(crate) struct Slots {
    cap: usize,
    held: Mutex<HashMap<Uuid, usize>>,
}

/// One held slot, given back when dropped.
pub(crate) struct Slot {
    slots: Arc<Slots>,
    key: Uuid,
}

impl Slots {
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap,
            held: Mutex::new(HashMap::new()),
        })
    }

    /// A slot for `key`, or `None` when it holds `cap` already.
    pub(crate) fn try_take(self: &Arc<Self>, key: Uuid) -> Option<Slot> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = held.entry(key).or_insert(0);
        if *n >= self.cap {
            if *n == 0 {
                held.remove(&key);
            }
            return None;
        }
        *n += 1;
        Some(Slot {
            slots: self.clone(),
            key,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = self
            .slots
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = held.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.remove(&self.key);
            }
        }
    }
}

impl std::fmt::Debug for Meetings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meetings")
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

impl Meetings {
    /// Builds the service over the runtime pool, the accounts kit, a media store, the media
    /// tools and a transcription provider.
    pub fn new(
        db: PgPool,
        accounts: Accounts,
        store: FileStore,
        tools: Tools,
        transcriber: Arc<dyn Transcriber>,
        settings: Settings,
    ) -> Self {
        let uploads = Arc::new(Semaphore::new(settings.uploads_at_once.max(1))); // LIMIT(uploads-at-once)
        let workspace_uploads = Slots::new(settings.uploads_per_workspace.max(1)); // LIMIT(uploads-per-workspace)
        let user_uploads = Slots::new(settings.uploads_per_user.max(1)); // LIMIT(uploads-per-user)
        Self {
            inner: Arc::new(Inner {
                db,
                accounts,
                store,
                tools,
                transcriber,
                settings,
                uploads,
                workspace_uploads,
                user_uploads,
            }),
        }
    }

    /// The media store.
    pub fn store(&self) -> &FileStore {
        &self.inner.store
    }

    /// The settings.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }
}
