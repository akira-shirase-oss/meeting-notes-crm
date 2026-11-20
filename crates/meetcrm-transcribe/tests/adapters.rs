//! The hosted adapter against a local stand-in for the provider, and the recorded adapter's
//! scripted faults.
#![expect(clippy::unwrap_used, reason = "integration test helpers")]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use meetcrm_transcribe::hosted::{ConfigError, Hosted};
use meetcrm_transcribe::recorded::{ANY, Fault, Recorded};
use meetcrm_transcribe::{ChunkAudio, TranscribeError, Transcriber, Utterance};

const KEY: &str = "dg-test-key-not-real";
const SHA: &str = "aa00000000000000000000000000000000000000000000000000000000000000";

/// What the fake provider saw: authorization, content type, query and body length.
type SeenRequest = (String, String, String, usize);

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<SeenRequest>>>);

/// A provider stand-in answering every request with `status` and `body` after `delay`.
async fn provider(status: StatusCode, body: Vec<u8>, delay: Duration) -> (SocketAddr, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Seen::default();
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            let record = record.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let body = body.clone();
                    let record = record.clone();
                    async move {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .map(|v| v.to_str().unwrap().to_owned())
                            .unwrap_or_default();
                        let ctype = req
                            .headers()
                            .get("content-type")
                            .map(|v| v.to_str().unwrap().to_owned())
                            .unwrap_or_default();
                        let query = req.uri().query().unwrap_or_default().to_owned();
                        let len = req
                            .into_body()
                            .collect()
                            .await
                            .map_or(0, |b| b.to_bytes().len());
                        record.0.lock().unwrap().push((auth, ctype, query, len));
                        tokio::time::sleep(delay).await;
                        let mut r = Response::new(Full::new(Bytes::from(body)));
                        *r.status_mut() = status;
                        Ok::<_, Infallible>(r)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, seen)
}

fn audio() -> PathBuf {
    let path = std::env::temp_dir().join(format!("meetcrm-chunk-{}.flac", std::process::id()));
    std::fs::write(&path, b"fLaC not really audio").unwrap();
    path
}

fn answer(duration: f64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "metadata": {"duration": duration},
        "results": {"utterances": [
            {"start": 0.1, "end": 2.0, "speaker": 0, "transcript": "Thanks for making the time."},
            {"start": 2.4, "end": 4.9, "speaker": 1, "transcript": "Happy to."}
        ]}
    }))
    .unwrap()
}

async fn ask(addr: SocketAddr, deadline: Duration) -> Result<Vec<Utterance>, TranscribeError> {
    let hosted = Hosted::new(&format!("http://{addr}"), KEY, deadline).unwrap();
    let path = audio();
    let chunk = ChunkAudio {
        path: &path,
        duration_ms: 5_000,
        recording: SHA,
        index: 0,
    };
    hosted.transcribe(chunk).await
}

#[tokio::test]
async fn a_diarised_answer_comes_back_as_speaker_labelled_utterances() {
    let (addr, seen) = provider(StatusCode::OK, answer(5.0), Duration::ZERO).await;
    let got = ask(addr, Duration::from_secs(10)).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].speaker, got[1].speaker), (0, 1));
    let seen = seen.0.lock().unwrap();
    let (auth, ctype, query, len) = &seen[0];
    assert_eq!(auth, &format!("Token {KEY}"));
    assert_eq!(ctype, "audio/flac");
    assert!(
        query.contains("diarize=true") && query.contains("utterances=true"),
        "{query}"
    );
    assert_eq!(*len, b"fLaC not really audio".len());
}

#[tokio::test]
async fn provider_failures_become_typed_errors() {
    let cases = [
        (
            StatusCode::SERVICE_UNAVAILABLE,
            TranscribeError::Unavailable,
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            TranscribeError::Unavailable,
        ),
        (StatusCode::TOO_MANY_REQUESTS, TranscribeError::RateLimited),
        (StatusCode::PAYMENT_REQUIRED, TranscribeError::RateLimited),
        (StatusCode::BAD_REQUEST, TranscribeError::Rejected),
        (StatusCode::UNAUTHORIZED, TranscribeError::Rejected),
    ];
    for (status, want) in cases {
        let (addr, _) = provider(status, b"{}".to_vec(), Duration::ZERO).await;
        assert_eq!(
            ask(addr, Duration::from_secs(10)).await,
            Err(want),
            "{status}"
        );
    }
    let (addr, _) = provider(StatusCode::OK, b"<html>".to_vec(), Duration::ZERO).await;
    assert_eq!(
        ask(addr, Duration::from_secs(10)).await,
        Err(TranscribeError::Malformed)
    );
    let huge = vec![b' '; meetcrm_transcribe::import::MAX_ANSWER_BYTES + 1];
    let (addr, _) = provider(StatusCode::OK, huge, Duration::ZERO).await;
    assert_eq!(
        ask(addr, Duration::from_secs(10)).await,
        Err(TranscribeError::Malformed)
    );
}

#[tokio::test]
async fn a_partial_answer_is_partial() {
    let (addr, _) = provider(StatusCode::OK, answer(2.5), Duration::ZERO).await;
    assert_eq!(
        ask(addr, Duration::from_secs(10)).await,
        Err(TranscribeError::Partial)
    );
}

#[tokio::test]
async fn a_provider_that_does_not_answer_times_out() {
    let (addr, _) = provider(StatusCode::OK, answer(5.0), Duration::from_secs(120)).await;
    let asked = tokio::time::timeout(
        Duration::from_secs(30),
        ask(addr, Duration::from_millis(300)),
    )
    .await;
    assert_eq!(asked, Ok(Err(TranscribeError::Timeout)));
}

#[tokio::test]
async fn audio_past_the_upload_cap_is_never_sent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let path = std::env::temp_dir().join(format!("meetcrm-big-{}.flac", std::process::id()));
    // Sparse: the size is what is checked, before a byte is read.
    std::fs::File::create(&path)
        .unwrap()
        .set_len(meetcrm_transcribe::hosted::MAX_AUDIO_BYTES + 1)
        .unwrap();
    let hosted = Hosted::new(&format!("http://{addr}"), KEY, Duration::from_secs(5)).unwrap();
    let chunk = ChunkAudio {
        path: &path,
        duration_ms: 5_000,
        recording: SHA,
        index: 0,
    };
    assert_eq!(
        hosted.transcribe(chunk).await,
        Err(TranscribeError::Rejected)
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn an_unreachable_provider_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    assert_eq!(
        ask(addr, Duration::from_secs(5)).await,
        Err(TranscribeError::Unavailable)
    );
}

#[test]
fn the_key_never_leaves_in_debug_and_plain_http_is_loopback_only() {
    let hosted = Hosted::new("https://api.deepgram.com", KEY, Duration::from_secs(1)).unwrap();
    assert!(!format!("{hosted:?}").contains(KEY));
    assert_eq!(
        Hosted::new("http://api.deepgram.com", KEY, Duration::from_secs(1)).err(),
        Some(ConfigError::Insecure)
    );
    assert_eq!(
        Hosted::new("http://10.0.0.1", KEY, Duration::from_secs(1)).err(),
        Some(ConfigError::Insecure)
    );
    assert_eq!(
        Hosted::new("ftp://x", KEY, Duration::from_secs(1)).err(),
        Some(ConfigError::Url)
    );
    assert_eq!(
        Hosted::new("https://api.deepgram.com", "", Duration::from_secs(1)).err(),
        Some(ConfigError::NoKey)
    );
    assert!(Hosted::new("http://[::1]:63690", KEY, Duration::from_secs(1)).is_ok());
}

#[tokio::test]
async fn the_recorded_adapter_replays_answers_and_scripted_faults_in_order() {
    let dir = std::env::temp_dir().join(format!("meetcrm-recorded-{}", std::process::id()));
    let recorded = Recorded::new(&dir);
    std::fs::create_dir_all(recorded.answer_path(SHA, 0).parent().unwrap()).unwrap();
    std::fs::write(recorded.answer_path(SHA, 0), answer(5.0)).unwrap();
    let path = audio();
    let chunk = ChunkAudio {
        path: &path,
        duration_ms: 5_000,
        recording: SHA,
        index: 0,
    };
    recorded.fault(
        &Recorded::key(SHA, 0),
        Fault::Fail(TranscribeError::Unavailable),
    );
    recorded.fault(ANY, Fault::Answer(answer(1.0)));
    assert_eq!(
        recorded.transcribe(chunk).await,
        Err(TranscribeError::Unavailable)
    );
    assert_eq!(
        recorded.transcribe(chunk).await,
        Err(TranscribeError::Partial)
    );
    assert_eq!(recorded.transcribe(chunk).await.unwrap().len(), 2);
    let missing = ChunkAudio { index: 1, ..chunk };
    assert_eq!(
        recorded.transcribe(missing).await,
        Err(TranscribeError::Rejected)
    );
    let traversal = ChunkAudio {
        recording: "../../etc",
        ..chunk
    };
    assert_eq!(
        recorded.transcribe(traversal).await,
        Err(TranscribeError::Rejected)
    );
    assert_eq!(recorded.calls(), 5);
    std::fs::remove_dir_all(dir).unwrap();
}
