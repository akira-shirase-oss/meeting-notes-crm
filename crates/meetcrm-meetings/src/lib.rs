//! Meetings: a meeting, its consent attestation, the consent-gated upload, the transcode
//! and transcribe jobs, the speaker-labelled transcript and deletion.
//!
//! The pipeline is a table of jobs (`jobs`, one per meeting and stage) claimed under a
//! lease, so a crashed worker's job is taken over when its lease runs out. Each stage
//! writes its output in the same transaction that finishes it, after checking it still
//! holds the lease; a transcribed chunk is marked in that transaction, so a retried job
//! never applies a chunk twice. Files follow the rows: they are placed while the
//! meeting's row is locked and removed through `media_garbage`, which a trigger fills
//! whenever a meeting row goes, so a deleted meeting (or workspace) takes its media with it.

pub mod error;
pub mod routes;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use meetcrm_accounts::Accounts;
use meetcrm_media::MediaLimits;
use meetcrm_media::store::FileStore;
use meetcrm_media::tools::Tools;
use meetcrm_transcribe::Transcriber;
use sqlx::PgPool;
use tokio::sync::Semaphore;

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
        Self {
            inner: Arc::new(Inner {
                db,
                accounts,
                store,
                tools,
                transcriber,
                settings,
                uploads,
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
