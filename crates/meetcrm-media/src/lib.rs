//! Recordings as files, and the only code that runs ffprobe and ffmpeg.
//!
//! An upload is streamed into a staging file under a byte cap, hashed on the way, probed,
//! and only then moved under its meeting's key: a refused file never reaches the store
//! ([`store`]). ffprobe's JSON is parsed by [`probe::parse`], which trusts nothing in it.
//! Both tools run through [`tools::Tools`]: an argument vector (never a shell), a cleared
//! environment, `prlimit` caps on CPU time, output file size and open files, a wall-clock
//! deadline that kills the process, only the `file` protocol and a fixed list of demuxers
//! (so a playlist cannot make ffmpeg fetch a URL or read another file).

pub mod probe;
pub mod store;
pub mod tools;

use std::time::Duration;

/// The bounds on one recording.
#[derive(Debug, Clone, Copy)]
pub struct MediaLimits {
    /// The largest upload accepted, in bytes.
    pub max_upload_bytes: u64,
    /// The longest recording accepted.
    pub max_duration: Duration,
    /// The wall-clock deadline for one ffprobe run.
    pub probe_deadline: Duration,
    /// The wall-clock deadline for one transcode.
    pub transcode_deadline: Duration,
    /// CPU seconds one ffmpeg or ffprobe process may use.
    pub cpu_seconds: u64,
    /// The largest file a transcode may write, and the cap on all its chunks together.
    pub max_output_bytes: u64,
    /// The length of one transcription chunk.
    pub chunk: Duration,
    /// The most chunks one recording may become.
    pub max_chunks: u32,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 256 * 1024 * 1024,
            max_duration: Duration::from_hours(4),
            probe_deadline: Duration::from_secs(10),
            transcode_deadline: Duration::from_secs(600),
            cpu_seconds: 600,
            max_output_bytes: 512 * 1024 * 1024,
            chunk: Duration::from_mins(10),
            max_chunks: 48,
        }
    }
}

/// Why a recording was refused or could not be processed. The codes are stable: they are
/// stored as a meeting's status reason and returned to the client.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone, Copy)]
pub enum MediaError {
    /// The bytes are not a recording ffprobe can read in an allowed container.
    #[error("media_corrupt")]
    Corrupt,
    /// No audio stream.
    #[error("media_no_audio")]
    NoAudio,
    /// Longer than [`MediaLimits::max_duration`].
    #[error("media_too_long")]
    TooLong,
    /// Larger than [`MediaLimits::max_upload_bytes`].
    #[error("media_too_large")]
    TooLarge,
    /// The tool ran past its wall-clock deadline and was killed.
    #[error("media_deadline")]
    Deadline,
    /// The tool used its CPU-time cap and was killed.
    #[error("media_cpu_limit")]
    CpuLimit,
    /// The tool tried to write more than the output cap.
    #[error("media_output_limit")]
    OutputLimit,
    /// The tool could not be started, or the store failed: our fault, retried.
    #[error("media_unavailable")]
    Unavailable,
}

impl MediaError {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Corrupt => "media_corrupt",
            Self::NoAudio => "media_no_audio",
            Self::TooLong => "media_too_long",
            Self::TooLarge => "media_too_large",
            Self::Deadline => "media_deadline",
            Self::CpuLimit => "media_cpu_limit",
            Self::OutputLimit => "media_output_limit",
            Self::Unavailable => "media_unavailable",
        }
    }

    /// Whether trying again could succeed (the recording itself is not at fault).
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}
