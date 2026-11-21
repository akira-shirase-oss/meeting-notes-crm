//! Configuration: typed, validated once at startup, every problem reported at once.
//!
//! Settings come from the environment (`deploy/env.example` lists every one with its
//! default and bounds; a test keeps the two in step). Secrets never do in production: a
//! secret named `x` is read from the systemd credential `$CREDENTIALS_DIRECTORY/x`
//! (`LoadCredential=` in the unit), else from the file named by `MEETCRM_X_FILE`, else —
//! for development only — from `MEETCRM_X`. A secret file must be a regular file of at
//! most 4 KiB; one named by `_FILE` must also be readable by its owner only (mode `0600` or
//! `0400`). systemd's credential directory is private to the service already.
//!
//! Loading reads files, so it runs before the async runtime starts.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use meetcrm_accounts::Settings;
use meetcrm_accounts::domain::PlanLimits;
use meetcrm_db::PoolSettings;
use meetcrm_http::ConnectionLimits;
use meetcrm_http::stack::StackLimits;

/// Every non-secret setting: name, default, bounds (as documented in `deploy/env.example`).
pub const VARIABLES: &[(&str, &str, &str)] = &[
    (
        "MEETCRM_LISTEN",
        "127.0.0.1:63650",
        "a socket address; keep it on loopback behind Caddy",
    ),
    ("MEETCRM_HEADER_READ_TIMEOUT_MS", "10000", "100..=120000"),
    (
        "MEETCRM_MAX_CONNECTIONS",
        "512",
        "1..=65536, below the unit's LimitNOFILE",
    ),
    ("MEETCRM_WRITE_STALL_MS", "10000", "100..=120000"),
    ("MEETCRM_MAX_REQUESTS_PER_CONNECTION", "1000", "1..=1000000"),
    ("MEETCRM_CONNECTION_MAX_AGE_MS", "600000", "1000..=86400000"),
    ("MEETCRM_REQUEST_GRACE_MS", "10000", "100..=120000"),
    (
        "MEETCRM_DRAIN_TIMEOUT_MS",
        "15000",
        "100..=300000, below the unit's TimeoutStopSec",
    ),
    ("MEETCRM_REQUEST_TIMEOUT_MS", "5000", "100..=120000"),
    ("MEETCRM_MAX_BODY_BYTES", "65536", "1024..=16777216"),
    ("MEETCRM_MAX_IN_FLIGHT", "64", "1..=100000"),
    ("MEETCRM_DB_MAX_CONNECTIONS", "16", "1..=1000"),
    ("MEETCRM_DB_ACQUIRE_TIMEOUT_MS", "2000", "100..=60000"),
    ("MEETCRM_DB_STATEMENT_TIMEOUT_MS", "3000", "100..=600000"),
    ("MEETCRM_READY_TIMEOUT_MS", "500", "50..=10000"),
    ("MEETCRM_STARTUP_TIMEOUT_MS", "30000", "1000..=600000"),
    (
        "MEETCRM_ERROR_RATE_PERMILLE",
        "50",
        "1..=1000: /errorz answers 503 above it",
    ),
    (
        "MEETCRM_ERROR_MIN_REQUESTS",
        "20",
        "1..=1000000: fewer requests in the window never alert",
    ),
    ("MEETCRM_LOG", "info", "a tracing filter"),
    ("MEETCRM_LOG_FORMAT", "json", "json or text"),
    (
        "MEETCRM_SMTP_RELAY",
        "",
        "a loopback socket address; empty disables mail",
    ),
    (
        "MEETCRM_MAIL_FROM",
        "noreply@meeting-notes-crm.invalid",
        "an email address",
    ),
    (
        "MEETCRM_PUBLIC_URL",
        "http://127.0.0.1:63650",
        "the http(s) origin links point to, no trailing slash",
    ),
    (
        "MEETCRM_COOKIE_SECURE",
        "true",
        "true or false (false only for plain-HTTP development)",
    ),
    ("MEETCRM_SESSION_TTL_S", "604800", "60..=31536000"),
    ("MEETCRM_SESSION_IDLE_S", "86400", "60..=31536000"),
    (
        "MEETCRM_TRUSTED_PROXIES",
        "",
        "comma-separated IP addresses (Caddy on this host: 127.0.0.1)",
    ),
    ("MEETCRM_PASSWORD_CONCURRENCY", "2", "1..=64"),
    ("MEETCRM_PLAN_MAX_MEMBERS", "10", "0..=100000"),
    ("MEETCRM_PLAN_MAX_TOKENS", "10", "0..=100000"),
    ("MEETCRM_PLAN_MAX_OWNED_WORKSPACES", "3", "0..=100000"),
    (
        "MEETCRM_MEDIA_DIR",
        "/var/lib/meetcrm/media",
        "a directory the server owns",
    ),
    (
        "MEETCRM_FFMPEG_DIR",
        "/opt/meetcrm/ffmpeg",
        "the pinned ffmpeg and ffprobe",
    ),
    ("MEETCRM_PRLIMIT", "/usr/bin/prlimit", "util-linux prlimit"),
    ("MEETCRM_TRANSCRIBER", "hosted", "hosted or recorded"),
    (
        "MEETCRM_RECORDED_DIR",
        "/var/lib/meetcrm/recorded",
        "the recorded provider's answers",
    ),
    (
        "MEETCRM_TRANSCRIBE_URL",
        "https://api.deepgram.com",
        "the hosted provider's origin",
    ),
    ("MEETCRM_UPLOAD_MAX_BYTES", "268435456", "1024..=2147483648"),
    ("MEETCRM_UPLOAD_TIMEOUT_S", "1800", "10..=7200"),
    ("MEETCRM_UPLOAD_IDLE_MS", "30000", "100..=600000"),
    ("MEETCRM_UPLOADS_AT_ONCE", "4", "1..=64"),
    ("MEETCRM_UPLOADS_PER_WORKSPACE", "2", "1..=64"),
    ("MEETCRM_UPLOADS_PER_USER", "1", "1..=64"),
    ("MEETCRM_UPLOAD_MIN_BYTES_PER_S", "16384", "1..=104857600"),
    ("MEETCRM_UPLOAD_GRACE_S", "10", "1..=600"),
    ("MEETCRM_WORKSPACE_MAX_MEETINGS", "10000", "1..=10000000"),
    (
        "MEETCRM_WORKSPACE_MAX_MEDIA_BYTES",
        "2147483648",
        "1024..=1099511627776",
    ),
    (
        "MEETCRM_MEDIA_MAX_MEMORY_BYTES",
        "536870912",
        "67108864..=4294967296",
    ),
    ("MEETCRM_MEDIA_MAX_DURATION_S", "14400", "1..=86400"),
    ("MEETCRM_WORKERS", "2", "1..=64"),
];

/// Every secret: credential name, the variable it may also come from, what it is.
pub const SECRETS: &[(&str, &str, &str)] = &[
    (
        "database-url",
        "MEETCRM_DATABASE_URL",
        "the server's database URL, as the runtime role",
    ),
    (
        "migration-database-url",
        "MEETCRM_MIGRATION_DATABASE_URL",
        "meetcrmctl migrate, digests, prune-events: the owner role",
    ),
    (
        "admin-database-url",
        "MEETCRM_ADMIN_DATABASE_URL",
        "meetcrmctl bootstrap: a role that may create roles and databases",
    ),
    (
        "owner-password",
        "MEETCRM_OWNER_PASSWORD",
        "meetcrmctl bootstrap: the owner role's password",
    ),
    (
        "app-password",
        "MEETCRM_APP_PASSWORD",
        "meetcrmctl bootstrap: the runtime role's password",
    ),
    (
        "transcribe-key",
        "MEETCRM_TRANSCRIBE_KEY",
        "the hosted transcription provider's API key (MEETCRM_TRANSCRIBER=hosted)",
    ),
];

/// A secret that never prints.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one place that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line (journald keeps it as is).
    Json,
    /// Human-readable, for development.
    Text,
}

/// The server's configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where to listen.
    pub listen: SocketAddr,
    /// The database, as the runtime role.
    pub database_url: Secret,
    /// The accept loop's bounds.
    pub connection: ConnectionLimits,
    /// The middleware stack's bounds.
    pub stack: StackLimits,
    /// Requests running at once on `/v1` before shedding.
    pub max_in_flight: usize,
    /// The pool.
    pub db: PoolSettings,
    /// The readiness probe's deadline.
    pub ready_timeout: Duration,
    /// How long startup waits for an unavailable database.
    pub startup_timeout: Duration,
    /// `/errorz` threshold, server errors per thousand requests.
    pub error_rate_permille: u64,
    /// Fewer requests than this in the window never alert.
    pub error_min_requests: u64,
    /// The log filter.
    pub log_filter: String,
    /// The log format.
    pub log_format: LogFormat,
    /// The mail relay; `None` disables mail.
    pub smtp_relay: Option<SocketAddr>,
    /// The accounts kit.
    pub accounts: Settings,
    /// Media, the pipeline and the transcription provider.
    pub media: MediaConfig,
}

/// Media, the upload route, the pipeline workers and the provider.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// The media store's root.
    pub dir: PathBuf,
    /// Holds `ffmpeg` and `ffprobe`.
    pub ffmpeg_dir: PathBuf,
    /// `prlimit`.
    pub prlimit: PathBuf,
    /// The provider.
    pub transcriber: TranscriberConfig,
    /// The upload route's request deadline and body cap.
    pub upload: StackLimits,
    /// The pipeline.
    pub meetings: meetcrm_meetings::Settings,
}

/// Which transcription adapter runs.
#[derive(Debug, Clone)]
pub enum TranscriberConfig {
    /// Saved answers from a directory (tests, demos, load runs).
    Recorded(PathBuf),
    /// The hosted provider.
    Hosted {
        /// Its origin.
        url: String,
        /// Its key.
        key: Secret,
    },
}

/// Where settings and secrets come from (the process, or a test's map).
pub trait Source {
    /// A variable, `None` when unset or empty.
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct Process;

impl Source for Process {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

impl Source for BTreeMap<&str, &str> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name)
            .filter(|v| !v.is_empty())
            .map(|v| (*v).to_owned())
    }
}

fn default_of(name: &str) -> &'static str {
    VARIABLES
        .iter()
        .find(|(n, _, _)| *n == name)
        .map_or("", |(_, d, _)| d)
}

/// The problems found so far, all reported together.
struct Problems(Vec<String>);

impl Problems {
    fn number<T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display>(
        &mut self,
        source: &dyn Source,
        name: &str,
        range: std::ops::RangeInclusive<T>,
    ) -> T {
        let raw = source
            .var(name)
            .unwrap_or_else(|| default_of(name).to_owned());
        match raw.parse::<T>() {
            Ok(v) if range.contains(&v) => v,
            _ => {
                self.0.push(format!(
                    "{name} must be a number in {}..={}",
                    range.start(),
                    range.end()
                ));
                *range.start()
            }
        }
    }

    fn millis(
        &mut self,
        source: &dyn Source,
        name: &str,
        range: std::ops::RangeInclusive<u64>,
    ) -> Duration {
        Duration::from_millis(self.number(source, name, range))
    }

    fn secs(
        &mut self,
        source: &dyn Source,
        name: &str,
        range: std::ops::RangeInclusive<u64>,
    ) -> Duration {
        Duration::from_secs(self.number(source, name, range))
    }
}

/// Reads secret `name`: the systemd credential, else `MEETCRM_X_FILE`, else `MEETCRM_X`.
pub fn secret(
    source: &dyn Source,
    credential: &str,
    variable: &str,
) -> Result<Option<Secret>, String> {
    if let Some(dir) = source.var("CREDENTIALS_DIRECTORY") {
        let path = Path::new(&dir).join(credential);
        if path.exists() {
            // systemd made this directory for this service alone; how it sets each file's
            // owner and mode varies by version, so the mode is not ours to check here.
            return read_secret_file(&path, false).map(Some);
        }
    }
    if let Some(file) = source.var(&format!("{variable}_FILE")) {
        return read_secret_file(&PathBuf::from(file), true).map(Some);
    }
    Ok(source.var(variable).map(Secret))
}

fn read_secret_file(path: &Path, owner_only: bool) -> Result<Secret, String> {
    let shown = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let meta = std::fs::metadata(path).map_err(|e| format!("secret file {shown}: {e}"))?;
    if !meta.is_file() || meta.len() > 4096 {
        return Err(format!(
            "secret file {shown} must be a regular file of at most 4 KiB"
        ));
    }
    if owner_only && meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "secret file {shown} is readable by others (mode {:o}); chmod 600 it",
            meta.mode() & 0o777
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("secret file {shown}: {e}"))?;
    let value = text.trim().to_owned();
    if value.is_empty() {
        return Err(format!("secret file {shown} is empty"));
    }
    Ok(Secret(value))
}

fn path(source: &dyn Source, name: &str, p: &mut Problems) -> PathBuf {
    let raw = source
        .var(name)
        .unwrap_or_else(|| default_of(name).to_owned());
    if !raw.starts_with('/') {
        p.0.push(format!("{name} must be an absolute path"));
    }
    PathBuf::from(raw)
}

fn media(source: &dyn Source, p: &mut Problems) -> MediaConfig {
    // The production default is the hosted provider; `recorded` is for tests and drills.
    let transcriber = match source.var("MEETCRM_TRANSCRIBER").as_deref() {
        Some("recorded") => TranscriberConfig::Recorded(path(source, "MEETCRM_RECORDED_DIR", p)),
        None | Some("hosted") => {
            let url = source
                .var("MEETCRM_TRANSCRIBE_URL")
                .unwrap_or_else(|| default_of("MEETCRM_TRANSCRIBE_URL").to_owned());
            let key = match secret(source, "transcribe-key", "MEETCRM_TRANSCRIBE_KEY") {
                Ok(Some(key)) => key,
                Ok(None) => {
                    p.0.push("MEETCRM_TRANSCRIBER=hosted needs the key: credential transcribe-key, MEETCRM_TRANSCRIBE_KEY_FILE or MEETCRM_TRANSCRIBE_KEY".to_owned());
                    Secret(String::new())
                }
                Err(e) => {
                    p.0.push(e);
                    Secret(String::new())
                }
            };
            TranscriberConfig::Hosted { url, key }
        }
        Some(_) => {
            p.0.push("MEETCRM_TRANSCRIBER must be hosted or recorded".to_owned());
            TranscriberConfig::Recorded(PathBuf::new())
        }
    };
    let max_upload_bytes: u64 = p.number(source, "MEETCRM_UPLOAD_MAX_BYTES", 1024..=2_147_483_648);
    let upload = StackLimits {
        request_timeout: p.secs(source, "MEETCRM_UPLOAD_TIMEOUT_S", 10..=7200),
        max_body_bytes: usize::try_from(max_upload_bytes).unwrap_or(usize::MAX),
    };
    let upload_deadline = upload.request_timeout;
    let defaults = meetcrm_meetings::Settings::default();
    let meetings = meetcrm_meetings::Settings {
        media: meetcrm_media::MediaLimits {
            max_upload_bytes,
            max_duration: p.secs(source, "MEETCRM_MEDIA_MAX_DURATION_S", 1..=86_400),
            max_memory_bytes: p.number(
                source,
                "MEETCRM_MEDIA_MAX_MEMORY_BYTES",
                67_108_864..=4_294_967_296,
            ),
            ..defaults.media
        },
        upload_idle: p.millis(source, "MEETCRM_UPLOAD_IDLE_MS", 100..=600_000),
        uploads_at_once: p.number(source, "MEETCRM_UPLOADS_AT_ONCE", 1..=64),
        uploads_per_workspace: p.number(source, "MEETCRM_UPLOADS_PER_WORKSPACE", 1..=64),
        uploads_per_user: p.number(source, "MEETCRM_UPLOADS_PER_USER", 1..=64),
        upload_min_bytes_per_s: p.number(source, "MEETCRM_UPLOAD_MIN_BYTES_PER_S", 1..=104_857_600),
        upload_grace: p.secs(source, "MEETCRM_UPLOAD_GRACE_S", 1..=600),
        upload_deadline,
        max_meetings_per_workspace: p.number(
            source,
            "MEETCRM_WORKSPACE_MAX_MEETINGS",
            1..=10_000_000,
        ),
        max_media_bytes_per_workspace: p.number(
            source,
            "MEETCRM_WORKSPACE_MAX_MEDIA_BYTES",
            1024..=1_099_511_627_776,
        ),
        workers: p.number(source, "MEETCRM_WORKERS", 1..=64),
        ..defaults
    };
    MediaConfig {
        dir: path(source, "MEETCRM_MEDIA_DIR", p),
        ffmpeg_dir: path(source, "MEETCRM_FFMPEG_DIR", p),
        prlimit: path(source, "MEETCRM_PRLIMIT", p),
        transcriber,
        upload,
        meetings,
    }
}

impl Config {
    /// Reads and validates everything, reporting every problem at once.
    #[expect(
        clippy::too_many_lines,
        reason = "one place reads every setting, so every problem is reported together"
    )]
    pub fn load(source: &dyn Source) -> Result<Self, Vec<String>> {
        let mut p = Problems(Vec::new());
        let database_url = match secret(source, "database-url", "MEETCRM_DATABASE_URL") {
            Ok(Some(url)) => url,
            Ok(None) => {
                p.0.push("the database URL is required: credential database-url, MEETCRM_DATABASE_URL_FILE or MEETCRM_DATABASE_URL".to_owned());
                Secret(String::new())
            }
            Err(e) => {
                p.0.push(e);
                Secret(String::new())
            }
        };
        let listen = source
            .var("MEETCRM_LISTEN")
            .unwrap_or_else(|| default_of("MEETCRM_LISTEN").to_owned())
            .parse::<SocketAddr>()
            .unwrap_or_else(|_| {
                p.0.push("MEETCRM_LISTEN must be a socket address".to_owned());
                SocketAddr::from(([127, 0, 0, 1], 0))
            });
        let connection = ConnectionLimits {
            header_read_timeout: p.millis(source, "MEETCRM_HEADER_READ_TIMEOUT_MS", 100..=120_000),
            max_connections: p.number(source, "MEETCRM_MAX_CONNECTIONS", 1..=65_536),
            write_stall: p.millis(source, "MEETCRM_WRITE_STALL_MS", 100..=120_000),
            max_requests_per_connection: p.number(
                source,
                "MEETCRM_MAX_REQUESTS_PER_CONNECTION",
                1..=1_000_000,
            ),
            max_age: p.millis(source, "MEETCRM_CONNECTION_MAX_AGE_MS", 1_000..=86_400_000),
            request_grace: p.millis(source, "MEETCRM_REQUEST_GRACE_MS", 100..=120_000),
            drain_timeout: p.millis(source, "MEETCRM_DRAIN_TIMEOUT_MS", 100..=300_000),
        };
        let stack = StackLimits {
            request_timeout: p.millis(source, "MEETCRM_REQUEST_TIMEOUT_MS", 100..=120_000),
            max_body_bytes: p.number(source, "MEETCRM_MAX_BODY_BYTES", 1024..=16_777_216),
        };
        let max_in_flight = p.number(source, "MEETCRM_MAX_IN_FLIGHT", 1..=100_000);
        let db = PoolSettings {
            max_connections: p.number(source, "MEETCRM_DB_MAX_CONNECTIONS", 1..=1_000),
            acquire_timeout: p.millis(source, "MEETCRM_DB_ACQUIRE_TIMEOUT_MS", 100..=60_000),
            statement_timeout: p.millis(source, "MEETCRM_DB_STATEMENT_TIMEOUT_MS", 100..=600_000),
            ..PoolSettings::default()
        };
        let ready_timeout = p.millis(source, "MEETCRM_READY_TIMEOUT_MS", 50..=10_000);
        let startup_timeout = p.millis(source, "MEETCRM_STARTUP_TIMEOUT_MS", 1_000..=600_000);
        let error_rate_permille = p.number(source, "MEETCRM_ERROR_RATE_PERMILLE", 1..=1_000);
        let error_min_requests = p.number(source, "MEETCRM_ERROR_MIN_REQUESTS", 1..=1_000_000);
        let log_filter = source
            .var("MEETCRM_LOG")
            .unwrap_or_else(|| default_of("MEETCRM_LOG").to_owned());
        let log_format = match source.var("MEETCRM_LOG_FORMAT").as_deref() {
            None | Some("json") => LogFormat::Json,
            Some("text") => LogFormat::Text,
            Some(_) => {
                p.0.push("MEETCRM_LOG_FORMAT must be json or text".to_owned());
                LogFormat::Json
            }
        };
        let smtp_relay = match source.var("MEETCRM_SMTP_RELAY") {
            None => None,
            Some(raw) => match raw.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => Some(addr),
                _ => {
                    p.0.push("MEETCRM_SMTP_RELAY must be a loopback socket address (the adapter speaks no TLS)".to_owned());
                    None
                }
            },
        };
        let public_url = source
            .var("MEETCRM_PUBLIC_URL")
            .unwrap_or_else(|| default_of("MEETCRM_PUBLIC_URL").to_owned());
        if !(public_url.starts_with("https://") || public_url.starts_with("http://"))
            || public_url.ends_with('/')
            || public_url.chars().any(char::is_whitespace)
        {
            p.0.push(
                "MEETCRM_PUBLIC_URL must be an http(s) origin without a trailing slash".to_owned(),
            );
        }
        let mail_from = source
            .var("MEETCRM_MAIL_FROM")
            .unwrap_or_else(|| default_of("MEETCRM_MAIL_FROM").to_owned());
        if !mail_from.contains('@')
            || mail_from
                .chars()
                .any(|c| c.is_whitespace() || c == '<' || c == '>')
        {
            p.0.push("MEETCRM_MAIL_FROM must be an email address".to_owned());
        }
        let cookie_secure = match source.var("MEETCRM_COOKIE_SECURE").as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            Some(_) => {
                p.0.push("MEETCRM_COOKIE_SECURE must be true or false".to_owned());
                true
            }
        };
        let mut trusted_proxies = Vec::new();
        for raw in source
            .var("MEETCRM_TRUSTED_PROXIES")
            .unwrap_or_default()
            .split(',')
        {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            match raw.parse::<IpAddr>() {
                Ok(ip) => trusted_proxies.push(ip),
                Err(_) => p.0.push(
                    "MEETCRM_TRUSTED_PROXIES must be comma-separated IP addresses".to_owned(),
                ),
            }
        }
        let accounts = Settings {
            public_url,
            mail_from,
            cookie_secure,
            session_ttl: p.secs(source, "MEETCRM_SESSION_TTL_S", 60..=31_536_000),
            session_idle: p.secs(source, "MEETCRM_SESSION_IDLE_S", 60..=31_536_000),
            plan: PlanLimits {
                max_members: p.number(source, "MEETCRM_PLAN_MAX_MEMBERS", 0..=100_000),
                max_tokens: p.number(source, "MEETCRM_PLAN_MAX_TOKENS", 0..=100_000),
                max_owned_workspaces: p.number(
                    source,
                    "MEETCRM_PLAN_MAX_OWNED_WORKSPACES",
                    0..=100_000,
                ),
            },
            trusted_proxies,
            password_concurrency: p.number(source, "MEETCRM_PASSWORD_CONCURRENCY", 1..=64),
            ..Settings::default()
        };
        let media = media(source, &mut p);
        if stack.request_timeout > connection.request_grace {
            p.0.push("MEETCRM_REQUEST_GRACE_MS must be at least MEETCRM_REQUEST_TIMEOUT_MS, so an aged connection can finish its request".to_owned());
        }
        if p.0.is_empty() {
            Ok(Self {
                listen,
                database_url,
                connection,
                stack,
                max_in_flight,
                db,
                ready_timeout,
                startup_timeout,
                error_rate_permille,
                error_min_requests,
                log_filter,
                log_format,
                smtp_relay,
                accounts,
                media,
            })
        } else {
            Err(p.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            (
                "MEETCRM_DATABASE_URL",
                "postgres://meetcrm_app:pw@127.0.0.1:63632/app",
            ),
            ("MEETCRM_TRANSCRIBE_KEY", "provider-key-value"),
        ])
    }

    #[test]
    fn the_production_default_transcriber_is_the_hosted_provider_not_the_test_one() {
        let mut unkeyed = base();
        unkeyed.remove("MEETCRM_TRANSCRIBE_KEY");
        let problems = Config::load(&unkeyed).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains("transcribe-key")),
            "{problems:?}"
        );
        let config = Config::load(&base()).unwrap();
        assert!(
            matches!(config.media.transcriber, TranscriberConfig::Hosted { .. }),
            "{:?}",
            config.media.transcriber
        );
        assert!(!format!("{config:?}").contains("provider-key-value"));
        let mut recorded = unkeyed;
        recorded.insert("MEETCRM_TRANSCRIBER", "recorded");
        assert!(matches!(
            Config::load(&recorded).unwrap().media.transcriber,
            TranscriberConfig::Recorded(_)
        ));
    }

    #[test]
    fn the_upload_fairness_and_plan_settings_are_read_and_bounded() {
        let mut source = base();
        source.extend([
            ("MEETCRM_UPLOAD_TIMEOUT_S", "600"),
            ("MEETCRM_UPLOADS_PER_WORKSPACE", "3"),
            ("MEETCRM_UPLOADS_PER_USER", "2"),
            ("MEETCRM_UPLOAD_MIN_BYTES_PER_S", "4096"),
            ("MEETCRM_UPLOAD_GRACE_S", "20"),
            ("MEETCRM_WORKSPACE_MAX_MEETINGS", "500"),
            ("MEETCRM_WORKSPACE_MAX_MEDIA_BYTES", "1073741824"),
            ("MEETCRM_MEDIA_MAX_MEMORY_BYTES", "268435456"),
        ]);
        let m = Config::load(&source).unwrap().media.meetings;
        assert_eq!(m.uploads_per_workspace, 3);
        assert_eq!(m.uploads_per_user, 2);
        assert_eq!(m.upload_min_bytes_per_s, 4096);
        assert_eq!(m.upload_grace, Duration::from_secs(20));
        assert_eq!(m.upload_deadline, Duration::from_secs(600));
        assert_eq!(m.max_meetings_per_workspace, 500);
        assert_eq!(m.max_media_bytes_per_workspace, 1_073_741_824);
        assert_eq!(m.media.max_memory_bytes, 268_435_456);
        source.extend([
            ("MEETCRM_UPLOADS_PER_USER", "0"),
            ("MEETCRM_UPLOAD_MIN_BYTES_PER_S", "0"),
            ("MEETCRM_WORKSPACE_MAX_MEDIA_BYTES", "10"),
        ]);
        let problems = Config::load(&source).unwrap_err();
        for name in [
            "MEETCRM_UPLOADS_PER_USER",
            "MEETCRM_UPLOAD_MIN_BYTES_PER_S",
            "MEETCRM_WORKSPACE_MAX_MEDIA_BYTES",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(name)),
                "{name}: {problems:?}"
            );
        }
    }

    #[test]
    fn the_defaults_load_and_the_secret_never_prints() {
        let config = Config::load(&base()).unwrap();
        assert_eq!(config.listen.to_string(), "127.0.0.1:63650");
        assert_eq!(config.connection, ConnectionLimits::default());
        let shown = format!("{config:?}");
        assert!(!shown.contains("pw@"), "{shown}");
        assert!(shown.contains("Secret(<redacted>)"));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let source = BTreeMap::from([
            ("MEETCRM_MAX_CONNECTIONS", "0"),
            ("MEETCRM_LOG_FORMAT", "yaml"),
            ("MEETCRM_SMTP_RELAY", "203.0.113.5:25"),
            ("MEETCRM_PUBLIC_URL", "https://app.example/"),
            ("MEETCRM_TRUSTED_PROXIES", "127.0.0.1, nope"),
            ("MEETCRM_REQUEST_TIMEOUT_MS", "60000"),
        ]);
        let problems = Config::load(&source).unwrap_err();
        for expected in [
            "database URL is required",
            "MEETCRM_MAX_CONNECTIONS",
            "MEETCRM_LOG_FORMAT",
            "MEETCRM_SMTP_RELAY",
            "MEETCRM_PUBLIC_URL",
            "MEETCRM_TRUSTED_PROXIES",
            "MEETCRM_REQUEST_GRACE_MS",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "{expected} not reported in {problems:?}"
            );
        }
        assert!(problems.iter().all(|p| !p.contains("pw@")));
    }

    #[test]
    fn secrets_come_from_credentials_or_owner_only_files() {
        let dir =
            std::env::temp_dir().join(format!("meetcrm-config-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("database-url");
        std::fs::write(&file, "postgres://meetcrm_app:from-file@127.0.0.1/app\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let file_text = file.to_string_lossy().into_owned();
        let dir_text = dir.to_string_lossy().into_owned();
        // A _FILE path must be owner-only...
        let by_path = BTreeMap::from([
            ("MEETCRM_DATABASE_URL_FILE", file_text.as_str()),
            ("MEETCRM_TRANSCRIBER", "recorded"),
        ]);
        let open = Config::load(&by_path).unwrap_err();
        assert!(
            open.iter().any(|p| p.contains("readable by others")),
            "{open:?}"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Config::load(&by_path).unwrap().database_url.expose(),
            "postgres://meetcrm_app:from-file@127.0.0.1/app"
        );
        // ...while systemd's credential directory is systemd's to protect, and wins.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o440)).unwrap();
        let credential = BTreeMap::from([
            ("CREDENTIALS_DIRECTORY", dir_text.as_str()),
            ("MEETCRM_DATABASE_URL", "postgres://ignored@127.0.0.1/app"),
            ("MEETCRM_TRANSCRIBER", "recorded"),
        ]);
        assert_eq!(
            Config::load(&credential).unwrap().database_url.expose(),
            "postgres://meetcrm_app:from-file@127.0.0.1/app"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `deploy/env.example` documents exactly the variables the code reads.
    #[test]
    fn the_environment_template_lists_every_variable_and_no_other() {
        let template = include_str!("../../../deploy/env.example");
        let mut documented: Vec<&str> = template
            .lines()
            .filter_map(|l| l.trim_start_matches('#').trim().split_once('='))
            .map(|(name, _)| name.trim())
            .filter(|name| name.starts_with("MEETCRM_"))
            .collect();
        documented.sort_unstable();
        documented.dedup();
        let mut known: Vec<String> = VARIABLES.iter().map(|(n, _, _)| (*n).to_owned()).collect();
        for (_, variable, _) in SECRETS {
            known.push(format!("{variable}_FILE"));
        }
        known.sort();
        assert_eq!(
            documented, known,
            "deploy/env.example and config.rs disagree"
        );
        for (name, default, _) in VARIABLES {
            assert!(
                template
                    .lines()
                    .any(|l| l.trim_start_matches('#').trim() == format!("{name}={default}")),
                "deploy/env.example does not show {name}'s default {default:?}"
            );
        }
    }
}
