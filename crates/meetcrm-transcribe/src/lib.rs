//! Transcription behind a port. [`Transcriber`] takes one audio chunk and returns
//! speaker-labelled utterances or a typed [`TranscribeError`]; the pipeline never sees a
//! provider's wire format. The provider's answer is untrusted input: [`import`] parses it
//! with bounds and refuses a partial one. [`hosted::Hosted`] is Deepgram's pre-recorded
//! API with diarisation; [`recorded::Recorded`] replays saved answers, with scripted
//! faults, for tests and load runs (no live key exists in development or CI).

pub mod hosted;
pub mod import;
pub mod recorded;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

/// One stretch of speech by one speaker, relative to its chunk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Utterance {
    /// The provider's speaker number within the chunk, from 0.
    pub speaker: u32,
    /// Start, from the chunk's start.
    pub start_ms: u64,
    /// End, from the chunk's start.
    pub end_ms: u64,
    /// What was said, control characters removed.
    pub text: String,
}

/// The audio to transcribe.
#[derive(Debug, Clone, Copy)]
pub struct ChunkAudio<'a> {
    /// The FLAC file.
    pub path: &'a Path,
    /// Its length, as measured when it was cut.
    pub duration_ms: u64,
    /// SHA-256 of the uploaded recording, hex: names the recording to the recorded adapter.
    pub recording: &'a str,
    /// Its position in the recording.
    pub index: u32,
}

/// Why a chunk has no transcript. The codes are stable (a meeting's status reason).
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeError {
    /// The provider could not be reached or failed (5xx, connection).
    #[error("provider_unavailable")]
    Unavailable,
    /// Throttled or out of quota (429, 402).
    #[error("provider_rate_limited")]
    RateLimited,
    /// No answer within the deadline.
    #[error("provider_timeout")]
    Timeout,
    /// The provider refused this audio (another 4xx): trying again will not help.
    #[error("provider_rejected")]
    Rejected,
    /// The answer did not parse or broke a bound.
    #[error("provider_malformed")]
    Malformed,
    /// The answer covers only part of the audio.
    #[error("provider_partial")]
    Partial,
}

impl TranscribeError {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "provider_unavailable",
            Self::RateLimited => "provider_rate_limited",
            Self::Timeout => "provider_timeout",
            Self::Rejected => "provider_rejected",
            Self::Malformed => "provider_malformed",
            Self::Partial => "provider_partial",
        }
    }

    /// Whether the same request could succeed later.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::RateLimited | Self::Timeout | Self::Partial
        )
    }
}

/// A boxed future, so the port can be a trait object.
pub type Transcription<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Utterance>, TranscribeError>> + Send + 'a>>;

/// The port.
pub trait Transcriber: Send + Sync + 'static {
    /// Transcribes one chunk, with diarisation.
    fn transcribe<'a>(&'a self, chunk: ChunkAudio<'a>) -> Transcription<'a>;
}
