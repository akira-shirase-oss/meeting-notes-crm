//! Meetings in process: a fresh migrated database, the accounts kit and the meetings routes
//! behind the template's stack (the upload route under its own limits), the pinned ffmpeg,
//! a media store in a scratch directory and the recorded transcriber.
#![allow(dead_code, reason = "each test binary uses part of the harness")]
#![allow(
    unreachable_pub,
    reason = "a test module shared by several test binaries"
)]
#![expect(clippy::unwrap_used, reason = "test harness")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use meetcrm_accounts::mail::SmtpRelay;
use meetcrm_accounts::testing::SmtpSink;
use meetcrm_accounts::{Accounts, store, tokens};
use meetcrm_db::testing::TestDb;
use meetcrm_http::stack::{self, ErrorWindow, StackLimits};
use meetcrm_media::MediaLimits;
use meetcrm_media::store::FileStore;
use meetcrm_media::tools::Tools;
use meetcrm_meetings::worker::Outcome;
use meetcrm_meetings::{Meetings, Settings};
use meetcrm_transcribe::Transcriber;
use meetcrm_transcribe::recorded::Recorded;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;
use uuid::Uuid;

pub const PASSWORD: &str = "correct horse battery staple";
pub const PEER: &str = "203.0.113.1:1000";

pub fn bin_dir() -> PathBuf {
    std::env::var_os("MEETCRM_FFMPEG_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tools/bin"),
        PathBuf::from,
    )
}

pub fn tools() -> Tools {
    let dir = bin_dir();
    assert!(
        dir.join("ffmpeg").exists(),
        "the pinned ffmpeg is missing: run scripts/install-ffmpeg.sh"
    );
    Tools {
        ffmpeg: dir.join("ffmpeg"),
        ffprobe: dir.join("ffprobe"),
        prlimit: PathBuf::from("prlimit"),
    }
}

pub fn scratch(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meetcrm-{what}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A WAV tone made directly with the pinned ffmpeg.
pub fn tone(seconds: u32) -> Vec<u8> {
    let dir = scratch("tone");
    let path = dir.join("tone.wav");
    let status = std::process::Command::new(bin_dir().join("ffmpeg"))
        .args(["-v", "quiet", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .args([
            "-ar",
            "8000",
            "-ac",
            "1",
            "-fflags",
            "+bitexact",
            "-map_metadata",
            "-1",
            "-f",
            "wav",
        ])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    bytes
}

pub fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A provider answer as the hosted API sends it.
pub fn answer(duration_s: f64, utterances: &[(u32, f64, f64, &str)]) -> Vec<u8> {
    let utterances: Vec<Value> = utterances
        .iter()
        .map(|(speaker, start, end, text)| {
            json!({ "speaker": speaker, "start": start, "end": end, "transcript": text })
        })
        .collect();
    serde_json::to_vec(&json!({
        "metadata": { "duration": duration_s },
        "results": { "utterances": utterances }
    }))
    .unwrap()
}

/// Limits for test recordings: two-second chunks, a minute at most.
pub fn test_settings() -> Settings {
    Settings {
        media: MediaLimits {
            max_upload_bytes: 4 * 1024 * 1024,
            max_duration: Duration::from_mins(1),
            chunk: Duration::from_secs(2),
            max_chunks: 30,
            ..MediaLimits::default()
        },
        retry_base: Duration::ZERO,
        retry_max: Duration::ZERO,
        max_attempts: 3,
        ..Settings::default()
    }
}

pub struct Kit {
    pub db: TestDb,
    pub app: Router,
    pub sink: SmtpSink,
    pub accounts: Accounts,
    pub meetings: Meetings,
    pub recorded: Arc<Recorded>,
    pub answers: PathBuf,
    pub media: PathBuf,
}

pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Answer {
    pub fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }
}

/// A member signed in to their own workspace.
pub struct Person {
    pub user: Uuid,
    pub session: String,
    pub workspace: Uuid,
}

impl Kit {
    pub async fn new() -> Self {
        Self::with(|_| {}, None, None).await
    }

    pub async fn tuned(tune: impl FnOnce(&mut Settings)) -> Self {
        Self::with(tune, None, None).await
    }

    /// `transcriber` replaces the recorded adapter; `tools` the pinned ffmpeg.
    pub async fn with(
        tune: impl FnOnce(&mut Settings),
        transcriber: Option<Arc<dyn Transcriber>>,
        tools: Option<Tools>,
    ) -> Self {
        let db = TestDb::new().await;
        let sink = SmtpSink::start("127.0.0.1:0", None).await.unwrap();
        let mut account_settings = meetcrm_accounts::Settings {
            public_url: "https://app.example.test".to_owned(),
            cookie_secure: false,
            ..meetcrm_accounts::Settings::default()
        };
        // Every test person signs up from the one test address.
        account_settings.mail.per_address = account_settings.mail.global;
        let accounts = Accounts::new(
            db.pool.clone(),
            account_settings,
            Arc::new(SmtpRelay::new(sink.addr).unwrap()),
        )
        .await
        .unwrap();
        let mut settings = test_settings();
        tune(&mut settings);
        let answers = scratch("answers");
        let media = scratch("media");
        let recorded = Arc::new(Recorded::new(&answers));
        let transcriber = transcriber.unwrap_or_else(|| recorded.clone());
        let upload_limits = StackLimits {
            request_timeout: Duration::from_mins(5),
            max_body_bytes: usize::try_from(settings.media.max_upload_bytes).unwrap(),
        };
        let meetings = Meetings::new(
            db.pool.clone(),
            accounts.clone(),
            FileStore::open(&media).await.unwrap(),
            tools.unwrap_or_else(self::tools),
            transcriber,
            settings,
        );
        let errors = Arc::new(ErrorWindow::default());
        let main = Router::new().nest(
            "/v1",
            meetcrm_accounts::router(accounts.clone())
                .merge(meetcrm_meetings::router(meetings.clone())),
        );
        let uploads = Router::new().nest("/v1", meetcrm_meetings::upload_router(meetings.clone()));
        let app = stack::apply(main, StackLimits::default(), errors.clone()).merge(stack::apply(
            uploads,
            upload_limits,
            errors,
        ));
        Self {
            db,
            app,
            sink,
            accounts,
            meetings,
            recorded,
            answers,
            media,
        }
    }

    pub async fn send(&self, request: Request<Body>) -> Answer {
        let mut request = request;
        let peer: SocketAddr = PEER.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Answer {
            status,
            headers,
            body,
        }
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Answer {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(id) = session {
            request = request.header(header::COOKIE, format!("meetcrm_session={id}"));
        }
        let body = match body {
            Some(v) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        self.send(request.body(body).unwrap()).await
    }

    pub async fn token_call(&self, method: Method, path: &str, token: &str) -> Answer {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    pub async fn upload_body(&self, who: &Person, meeting: Uuid, body: Body) -> Answer {
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!(
                "/v1/workspaces/{}/meetings/{meeting}/media",
                who.workspace
            ))
            .header(header::COOKIE, format!("meetcrm_session={}", who.session))
            .body(body)
            .unwrap();
        self.send(request).await
    }

    /// Uploads `bytes`, declaring their length as a client does.
    pub async fn upload(&self, who: &Person, meeting: Uuid, bytes: Vec<u8>) -> Answer {
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!(
                "/v1/workspaces/{}/meetings/{meeting}/media",
                who.workspace
            ))
            .header(header::COOKIE, format!("meetcrm_session={}", who.session))
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap();
        self.send(request).await
    }

    /// A verified, signed-in user with a workspace of their own.
    pub async fn person(&self, email: &str) -> Person {
        let code = tokens::new_invite().unwrap();
        store::create_invites(
            &self.db.pool,
            &[tokens::digest(&code)],
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
        let up = self
            .call(
                Method::POST,
                "/v1/signup",
                None,
                Some(json!({ "invite": code, "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
        let token = self.sink.latest_to(email).await.unwrap().token().unwrap();
        let verified = self
            .call(
                Method::POST,
                "/v1/verify",
                None,
                Some(json!({ "token": token })),
            )
            .await;
        assert_eq!(verified.status, StatusCode::NO_CONTENT, "{}", verified.body);
        let user = up.body["user_id"].as_str().unwrap().parse().unwrap();
        let signed_in = self
            .call(
                Method::POST,
                "/v1/session",
                None,
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
        let cookie = signed_in
            .headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let session = cookie
            .strip_prefix("meetcrm_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let ws = self
            .call(
                Method::POST,
                "/v1/workspaces",
                Some(&session),
                Some(json!({ "name": "Team" })),
            )
            .await;
        assert_eq!(ws.status, StatusCode::CREATED, "{}", ws.body);
        Person {
            user,
            session,
            workspace: ws.body["id"].as_str().unwrap().parse().unwrap(),
        }
    }

    /// An API token of `who`'s workspace.
    pub async fn token(&self, who: &Person) -> String {
        let answer = self
            .call(
                Method::POST,
                &format!("/v1/workspaces/{}/tokens", who.workspace),
                Some(&who.session),
                Some(json!({ "name": "ci" })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        answer.body["token"].as_str().unwrap().to_owned()
    }

    pub async fn meeting(&self, who: &Person) -> Uuid {
        let answer = self
            .call(
                Method::POST,
                &format!("/v1/workspaces/{}/meetings", who.workspace),
                Some(&who.session),
                Some(json!({ "title": "Discovery call" })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        answer.body["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn consent(&self, who: &Person, meeting: Uuid) {
        let answer = self
            .call(
                Method::POST,
                &format!(
                    "/v1/workspaces/{}/meetings/{meeting}/consent",
                    who.workspace
                ),
                Some(&who.session),
                Some(json!({ "statement": "all_participants_consented", "participants": 2 })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    }

    /// A consented meeting with `bytes` uploaded.
    pub async fn uploaded(&self, who: &Person, bytes: Vec<u8>) -> Uuid {
        let meeting = self.meeting(who).await;
        self.consent(who, meeting).await;
        let answer = self.upload(who, meeting, bytes).await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.body);
        meeting
    }

    /// Saves the recorded provider's answer for chunk `index` of `recording`.
    pub fn record(&self, recording: &str, index: u32, body: &[u8]) {
        let path = self.recorded.answer_path(recording, index);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    pub async fn work(&self) -> Outcome {
        self.meetings.work_once().await.unwrap()
    }

    pub async fn show(&self, who: &Person, meeting: Uuid) -> Answer {
        self.call(
            Method::GET,
            &format!("/v1/workspaces/{}/meetings/{meeting}", who.workspace),
            Some(&who.session),
            None,
        )
        .await
    }

    pub async fn transcript(&self, who: &Person, meeting: Uuid) -> Answer {
        self.call(
            Method::GET,
            &format!(
                "/v1/workspaces/{}/meetings/{meeting}/transcript",
                who.workspace
            ),
            Some(&who.session),
            None,
        )
        .await
    }

    /// A new user added to `owner`'s workspace with `role`, acting in it.
    pub async fn join(&self, owner: &Person, email: &str, role: &str) -> Person {
        let joined = self.person(email).await;
        sqlx::query("INSERT INTO members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(owner.workspace)
            .bind(joined.user)
            .bind(role)
            .execute(&self.db.pool)
            .await
            .unwrap();
        Person {
            workspace: owner.workspace,
            ..joined
        }
    }

    /// Files in the media store, staging included.
    pub async fn files(&self) -> usize {
        self.meetings.store().file_count().await.unwrap()
    }

    pub async fn count(&self, sql: &'static str, meeting: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(meeting)
            .fetch_one(&self.db.pool)
            .await
            .unwrap()
    }
}
