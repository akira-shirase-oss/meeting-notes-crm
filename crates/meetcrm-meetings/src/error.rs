//! The meetings routes' errors, in the template's envelope.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use meetcrm_accounts::error::AccountsError;
use meetcrm_http::stack::error;
use meetcrm_media::MediaError;

/// What a meetings route or job can fail with.
#[derive(Debug, thiserror::Error)]
pub enum MeetingsError {
    /// Authentication, authorization, 404, database and internal errors, as the kit
    /// answers them.
    #[error(transparent)]
    Access(#[from] AccountsError),
    /// A malformed or out-of-bounds value (422), with its code.
    #[error("{1}")]
    Invalid(&'static str, &'static str),
    /// A state conflict (409), with its code.
    #[error("conflict {0}")]
    Conflict(&'static str),
    /// The recording was refused (413 or 422) or the tools could not run (503).
    #[error("media {0}")]
    Media(MediaError),
    /// Every upload slot is taken (503 with `Retry-After`).
    #[error("uploads busy")]
    Busy,
    /// The client stopped sending the body (408).
    #[error("upload stalled")]
    Stalled,
    /// The body arrived slower than the minimum rate (408).
    #[error("upload too slow")]
    TooSlow,
}

impl From<sqlx::Error> for MeetingsError {
    fn from(e: sqlx::Error) -> Self {
        Self::Access(e.into())
    }
}

impl From<MediaError> for MeetingsError {
    fn from(e: MediaError) -> Self {
        Self::Media(e)
    }
}

impl MeetingsError {
    pub(crate) fn not_found() -> Self {
        Self::Access(AccountsError::NotFound)
    }

    pub(crate) fn internal(what: &'static str) -> Self {
        Self::Access(AccountsError::Internal(anyhow::anyhow!(what)))
    }
}

impl IntoResponse for MeetingsError {
    fn into_response(self) -> Response {
        use StatusCode as S;
        match self {
            Self::Access(e) => e.into_response(),
            Self::Invalid(code, message) => error(S::UNPROCESSABLE_ENTITY, code, message),
            Self::Conflict(code) => error(S::CONFLICT, code, conflict_message(code)),
            Self::Media(MediaError::TooLarge) => error(
                S::PAYLOAD_TOO_LARGE,
                "media_too_large",
                "the recording is larger than allowed",
            ),
            Self::Media(MediaError::Unavailable) => error(
                S::SERVICE_UNAVAILABLE,
                "media_unavailable",
                "media processing is unavailable; retry later",
            ),
            Self::Media(e) => error(S::UNPROCESSABLE_ENTITY, e.code(), media_message(e)),
            Self::Busy => {
                let mut response = error(
                    S::SERVICE_UNAVAILABLE,
                    "uploads_busy",
                    "too many uploads in progress; retry later",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
                response
            }
            Self::Stalled => error(
                S::REQUEST_TIMEOUT,
                "upload_stalled",
                "the upload stopped sending",
            ),
            Self::TooSlow => error(
                S::REQUEST_TIMEOUT,
                "upload_too_slow",
                "the upload arrived slower than the minimum rate",
            ),
        }
    }
}

fn conflict_message(code: &str) -> &'static str {
    match code {
        "consent_required" => "record the participants' consent before uploading",
        "consent_exists" => "consent is already recorded for this meeting",
        "media_exists" => "this meeting already has its recording",
        "transcript_incomplete" => "the transcript is not complete",
        _ => "conflict",
    }
}

fn media_message(e: MediaError) -> &'static str {
    match e {
        MediaError::Corrupt => "the file is not a recording we can read",
        MediaError::NoAudio => "the file has no audio",
        MediaError::TooLong => "the recording is longer than allowed",
        MediaError::Deadline => "processing the recording took too long",
        MediaError::CpuLimit => "processing the recording used too much CPU",
        MediaError::Killed => "processing the recording was interrupted",
        MediaError::OutputLimit => "the recording would produce too much audio",
        MediaError::TooLarge | MediaError::Unavailable => "media refused",
    }
}
