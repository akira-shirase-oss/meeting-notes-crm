//! Deepgram's pre-recorded API (`POST /v1/listen`, `diarize=true&utterances=true`), the
//! hosted provider. One request per chunk, the FLAC as the body, the key in the
//! `Authorization` header (never logged, never in a URL, absent from `Debug`). The base
//! URL is configuration, not input: HTTPS, or plain HTTP to a loopback address (tests).
//! Every call has a deadline covering connect, upload and answer; the chunk is streamed from
//! its file a piece at a time (never held in memory whole), and the answer is read under
//! [`MAX_ANSWER_BYTES`].

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use hyper::body::{Bytes, Frame, SizeHint};
use hyper::{Request, StatusCode, Uri, header};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncRead as _, ReadBuf};

use crate::import::{self, MAX_ANSWER_BYTES};
use crate::{ChunkAudio, TranscribeError, Transcriber, Transcription};

/// The query: the model, diarisation and utterance grouping.
pub const QUERY: &str =
    "model=nova-3&diarize=true&utterances=true&punctuate=true&smart_format=true";

/// The largest chunk sent.
pub const MAX_AUDIO_BYTES: u64 = 512 * 1024 * 1024;

/// A configuration the adapter refuses.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Not an absolute URL.
    #[error("transcriber URL does not parse")]
    Url,
    /// Plain HTTP to anything but loopback.
    #[error("transcriber URL must be https (http only to loopback)")]
    Insecure,
    /// No key.
    #[error("transcriber key is empty")]
    NoKey,
}

/// The adapter.
pub struct Hosted {
    client: Client<HttpsConnector<HttpConnector>, FileBody>,
    endpoint: Uri,
    authorization: header::HeaderValue,
    deadline: Duration,
}

impl std::fmt::Debug for Hosted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosted")
            .field("endpoint", &self.endpoint)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl Hosted {
    /// An adapter for `base` (`https://api.deepgram.com`) with `key`.
    pub fn new(base: &str, key: &str, deadline: Duration) -> Result<Self, ConfigError> {
        let base: Uri = base
            .trim_end_matches('/')
            .parse()
            .map_err(|_| ConfigError::Url)?;
        let host = base.host().ok_or(ConfigError::Url)?;
        match base.scheme_str() {
            Some("https") => {}
            Some("http") => {
                let ip: IpAddr = host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse()
                    .map_err(|_| ConfigError::Insecure)?;
                if !ip.is_loopback() {
                    return Err(ConfigError::Insecure);
                }
            }
            _ => return Err(ConfigError::Url),
        }
        if key.is_empty() {
            return Err(ConfigError::NoKey);
        }
        let mut authorization = header::HeaderValue::from_str(&format!("Token {key}"))
            .map_err(|_| ConfigError::NoKey)?;
        authorization.set_sensitive(true);
        let endpoint: Uri = format!("{base}/v1/listen?{QUERY}")
            .parse()
            .map_err(|_| ConfigError::Url)?;
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(deadline.min(Duration::from_secs(10))));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .build(https);
        Ok(Self {
            client,
            endpoint,
            authorization,
            deadline,
        })
    }

    async fn call(&self, chunk: ChunkAudio<'_>) -> Result<Vec<crate::Utterance>, TranscribeError> {
        let size = tokio::fs::metadata(chunk.path)
            .await
            .map_err(|_| TranscribeError::Unavailable)?
            .len();
        // LIMIT(provider-upload)
        if size > MAX_AUDIO_BYTES {
            return Err(TranscribeError::Rejected);
        }
        let file = tokio::fs::File::open(chunk.path)
            .await
            .map_err(|_| TranscribeError::Unavailable)?;
        let request = Request::post(self.endpoint.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "audio/flac")
            .header(header::CONTENT_LENGTH, size)
            .body(FileBody {
                file,
                buf: vec![0; READ_BYTES].into_boxed_slice(),
                left: size,
            })
            .map_err(|_| TranscribeError::Unavailable)?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|_| TranscribeError::Unavailable)?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::TOO_MANY_REQUESTS | StatusCode::PAYMENT_REQUIRED => {
                return Err(TranscribeError::RateLimited);
            }
            StatusCode::REQUEST_TIMEOUT => return Err(TranscribeError::Timeout),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(TranscribeError::Unauthorized);
            }
            s if s.is_server_error() => return Err(TranscribeError::Unavailable),
            _ => return Err(TranscribeError::Rejected),
        }
        let body = Limited::new(response.into_body(), MAX_ANSWER_BYTES)
            .collect()
            .await
            .map_err(|e| {
                // Past the bound, the answer is malformed; a cut connection is retried.
                if e.downcast_ref::<LengthLimitError>().is_some() {
                    TranscribeError::Malformed
                } else {
                    TranscribeError::Unavailable
                }
            })?
            .to_bytes();
        import::deepgram(&body, chunk.duration_ms)
    }
}

/// The piece of a chunk read and sent at a time.
const READ_BYTES: usize = 64 * 1024;

/// A chunk file as a request body: `left` bytes, read `READ_BYTES` at a time.
struct FileBody {
    file: tokio::fs::File,
    buf: Box<[u8]>,
    left: u64,
}

impl hyper::body::Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = &mut *self;
        if this.left == 0 {
            return Poll::Ready(None);
        }
        let mut read = ReadBuf::new(&mut this.buf);
        match Pin::new(&mut this.file).poll_read(cx, &mut read) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Ok(())) => {
                let filled = read.filled();
                if filled.is_empty() {
                    return Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into())));
                }
                let n = filled
                    .len()
                    .min(usize::try_from(this.left).unwrap_or(usize::MAX));
                let piece = Bytes::copy_from_slice(&filled[..n]);
                this.left -= piece.len() as u64;
                Poll::Ready(Some(Ok(Frame::data(piece))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

impl Transcriber for Hosted {
    fn transcribe<'a>(&'a self, chunk: ChunkAudio<'a>) -> Transcription<'a> {
        Box::pin(async move {
            tokio::time::timeout(self.deadline, self.call(chunk)) // LIMIT(provider-deadline)
                .await
                .unwrap_or(Err(TranscribeError::Timeout))
        })
    }
}
