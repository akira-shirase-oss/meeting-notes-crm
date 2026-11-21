//! The recorded adapter: answers saved as the provider would send them, at
//! `<dir>/<recording sha-256 hex>/chunk-NNNN.json`, imported exactly as a live answer is.
//! Faults are scripted per chunk (or for every chunk) and consumed one per call, so a test
//! can make the first attempt fail and the retry succeed; every call is counted.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use crate::import::{self, MAX_ANSWER_BYTES};
use crate::{ChunkAudio, TranscribeError, Transcriber, Transcription};

/// A scripted misbehaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Fail with this error.
    Fail(TranscribeError),
    /// Never answer (the caller's deadline ends it).
    Hang,
    /// Answer with these bytes instead of the recording.
    Answer(Vec<u8>),
}

/// The adapter.
#[derive(Debug)]
pub struct Recorded {
    dir: PathBuf,
    faults: Mutex<HashMap<String, VecDeque<Fault>>>,
    calls: AtomicU64,
}

/// Every chunk.
pub const ANY: &str = "*";

impl Recorded {
    /// Replays answers from `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            faults: Mutex::new(HashMap::new()),
            calls: AtomicU64::new(0),
        }
    }

    /// The key of a chunk, for [`Recorded::fault`].
    pub fn key(recording: &str, index: u32) -> String {
        format!("{recording}/{index}")
    }

    /// Queues `fault` for the chunk `key` (or [`ANY`]).
    pub fn fault(&self, key: &str, fault: Fault) {
        if let Ok(mut faults) = self.faults.lock() {
            faults.entry(key.to_owned()).or_default().push_back(fault);
        }
    }

    /// How many calls were made.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// The path of a chunk's saved answer.
    pub fn answer_path(&self, recording: &str, index: u32) -> PathBuf {
        self.dir
            .join(recording)
            .join(format!("chunk-{index:04}.json"))
    }

    fn next_fault(&self, key: &str) -> Option<Fault> {
        let mut faults = self.faults.lock().ok()?;
        if let Some(f) = faults.get_mut(key).and_then(VecDeque::pop_front) {
            return Some(f);
        }
        faults.get_mut(ANY).and_then(VecDeque::pop_front)
    }
}

impl Transcriber for Recorded {
    fn transcribe<'a>(&'a self, chunk: ChunkAudio<'a>) -> Transcription<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let recording_ok = chunk.recording.len() == 64
                && chunk.recording.bytes().all(|b| b.is_ascii_hexdigit());
            if !recording_ok {
                return Err(TranscribeError::Rejected);
            }
            match self.next_fault(&Self::key(chunk.recording, chunk.index)) {
                Some(Fault::Fail(e)) => return Err(e),
                Some(Fault::Hang) => {
                    tokio::time::sleep(Duration::from_hours(24)).await;
                    return Err(TranscribeError::Timeout);
                }
                Some(Fault::Answer(body)) => return import::deepgram(&body, chunk.duration_ms),
                None => {}
            }
            let path = self.answer_path(chunk.recording, chunk.index);
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|_| TranscribeError::Rejected)?;
            let mut body = Vec::new();
            file.take(u64::try_from(MAX_ANSWER_BYTES).unwrap_or(u64::MAX) + 1)
                .read_to_end(&mut body)
                .await
                .map_err(|_| TranscribeError::Unavailable)?;
            import::deepgram(&body, chunk.duration_ms)
        })
    }
}
