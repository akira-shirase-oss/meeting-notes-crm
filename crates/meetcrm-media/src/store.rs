//! Recordings and their chunks as files under one root, keyed only by ids:
//!
//! ```text
//! <root>/staging/<uuid>/            an upload or a transcode in progress; never referenced
//! <root>/media/<workspace>/<meeting>/original
//! <root>/media/<workspace>/<meeting>/chunks/chunk-0000.flac …
//! ```
//!
//! No client text reaches a path. Staging is emptied on open (nothing in it is referenced
//! by the database), and a staging directory dropped without being placed removes itself,
//! so a cancelled upload leaves nothing behind.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::MediaError;

/// The store.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

/// A directory under staging, removed when dropped unless its contents were placed.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
    keep: bool,
}

impl Staging {
    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Removes it now.
    pub async fn discard(mut self) {
        self.keep = true;
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let dir = std::mem::take(&mut self.dir);
            handle.spawn(async move {
                let _ = tokio::fs::remove_dir_all(dir).await;
            });
        }
    }
}

/// An upload being received: bytes are counted and hashed as they are written.
#[derive(Debug)]
pub struct Incoming {
    staging: Staging,
    file: tokio::fs::File,
    hasher: Sha256,
    bytes: u64,
    cap: u64,
}

/// A received upload, still in staging.
#[derive(Debug)]
pub struct Received {
    /// Its staging directory; the file is `original` in it.
    pub staging: Staging,
    /// SHA-256 of the bytes.
    pub sha256: [u8; 32],
    /// How many.
    pub bytes: u64,
}

impl Received {
    /// The staged file.
    pub fn path(&self) -> PathBuf {
        self.staging.dir.join("original")
    }
}

impl Incoming {
    /// Appends `data`, refusing past the cap (the partial file goes with the staging).
    pub async fn write(&mut self, data: &[u8]) -> Result<(), MediaError> {
        let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.bytes = self.bytes.saturating_add(len);
        // LIMIT(upload-bytes)
        if self.bytes > self.cap {
            return Err(MediaError::TooLarge);
        }
        self.hasher.update(data);
        self.file
            .write_all(data)
            .await
            .map_err(|_| MediaError::Unavailable)
    }

    /// Flushes and syncs the file.
    pub async fn finish(mut self) -> Result<Received, MediaError> {
        self.file
            .flush()
            .await
            .map_err(|_| MediaError::Unavailable)?;
        self.file
            .sync_all()
            .await
            .map_err(|_| MediaError::Unavailable)?;
        Ok(Received {
            staging: self.staging,
            sha256: self.hasher.finalize().into(),
            bytes: self.bytes,
        })
    }
}

impl FileStore {
    /// Opens (creating) the store and empties staging.
    pub async fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        let staging = root.join("staging");
        match tokio::fs::remove_dir_all(&staging).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        tokio::fs::create_dir_all(&staging).await?;
        tokio::fs::create_dir_all(root.join("media")).await?;
        Ok(Self { root })
    }

    /// The root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A fresh staging directory.
    pub async fn staging(&self) -> Result<Staging, MediaError> {
        let dir = self.root.join("staging").join(Uuid::new_v4().to_string());
        tokio::fs::create_dir(&dir)
            .await
            .map_err(|_| MediaError::Unavailable)?;
        Ok(Staging { dir, keep: false })
    }

    /// Starts receiving an upload of at most `cap` bytes.
    pub async fn receive(&self, cap: u64) -> Result<Incoming, MediaError> {
        let staging = self.staging().await?;
        let file = tokio::fs::File::create(staging.dir.join("original"))
            .await
            .map_err(|_| MediaError::Unavailable)?;
        Ok(Incoming {
            staging,
            file,
            hasher: Sha256::new(),
            bytes: 0,
            cap,
        })
    }

    /// A meeting's directory.
    pub fn meeting_dir(&self, workspace: Uuid, meeting: Uuid) -> PathBuf {
        self.root
            .join("media")
            .join(workspace.to_string())
            .join(meeting.to_string())
    }

    /// A meeting's original recording.
    pub fn original(&self, workspace: Uuid, meeting: Uuid) -> PathBuf {
        self.meeting_dir(workspace, meeting).join("original")
    }

    /// A meeting's chunk directory.
    pub fn chunks(&self, workspace: Uuid, meeting: Uuid) -> PathBuf {
        self.meeting_dir(workspace, meeting).join("chunks")
    }

    /// Moves a received upload under its meeting.
    pub async fn place_original(
        &self,
        received: Received,
        workspace: Uuid,
        meeting: Uuid,
    ) -> Result<(), MediaError> {
        let dir = self.meeting_dir(workspace, meeting);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|_| MediaError::Unavailable)?;
        tokio::fs::rename(received.path(), dir.join("original"))
            .await
            .map_err(|_| MediaError::Unavailable)?;
        sync_dir(&dir).await;
        received.staging.discard().await;
        Ok(())
    }

    /// Moves transcoded chunks (the whole staging directory) under their meeting,
    /// replacing any earlier attempt's.
    pub async fn place_chunks(
        &self,
        mut staging: Staging,
        workspace: Uuid,
        meeting: Uuid,
    ) -> Result<(), MediaError> {
        let dir = self.meeting_dir(workspace, meeting);
        let target = self.chunks(workspace, meeting);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|_| MediaError::Unavailable)?;
        match tokio::fs::remove_dir_all(&target).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(MediaError::Unavailable),
            _ => {}
        }
        tokio::fs::rename(&staging.dir, &target)
            .await
            .map_err(|_| MediaError::Unavailable)?;
        staging.keep = true;
        sync_dir(&dir).await;
        Ok(())
    }

    /// Removes everything held for a meeting. Idempotent.
    pub async fn remove_meeting(&self, workspace: Uuid, meeting: Uuid) -> Result<(), MediaError> {
        match tokio::fs::remove_dir_all(self.meeting_dir(workspace, meeting)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(MediaError::Unavailable),
        }
    }

    /// How many files the store holds, staging included (for tests and drills).
    pub async fn file_count(&self) -> io::Result<usize> {
        let mut count = 0;
        let mut stack = vec![self.root.clone()];
        // A staging directory may vanish mid-walk (its drop removes it): skip what is gone.
        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            loop {
                let entry = match entries.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    Err(e) => return Err(e),
                };
                match entry.file_type().await {
                    Ok(t) if t.is_dir() => stack.push(entry.path()),
                    Ok(_) => count += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(count)
    }
}

async fn sync_dir(dir: &Path) {
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
}
