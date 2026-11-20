//! `meetcrm-server`: load the configuration (before the runtime: it reads secret files),
//! connect, refuse to run as a role or on a database that breaks the protections or the
//! schema contract, serve with every bound, drain.
//!
//! Exit codes: 0 drained cleanly; 1 failed, or the drain deadline cut connections;
//! 2 configuration refused; 3 the role or the protections refused (`meetcrm_db::guard`);
//! 4 the schema is not exactly this binary's (`meetcrm_db::schema`).

use std::process::ExitCode;
use std::sync::Arc;

use meetcrm_accounts::Accounts;
use meetcrm_accounts::mail::{Disabled, MailTransport, SmtpRelay};
use meetcrm_db::guard::{Refusal, check_at_startup};
use meetcrm_db::schema;
use meetcrm_http::stack::ErrorWindow;
use meetcrm_http::{Drained, serve};
use meetcrm_media::store::FileStore;
use meetcrm_media::tools::Tools;
use meetcrm_meetings::Meetings;
use meetcrm_server::config::{Config, LogFormat, Process, TranscriberConfig};
use meetcrm_server::health::Health;
use meetcrm_server::{AppState, app};
use meetcrm_transcribe::Transcriber;
use meetcrm_transcribe::hosted::Hosted;
use meetcrm_transcribe::recorded::Recorded;
use tokio::net::TcpListener;

fn main() -> ExitCode {
    let config = match Config::load(&Process) {
        Ok(config) => config,
        Err(problems) => {
            eprintln!("meetcrm-server: configuration refused:");
            for problem in problems {
                eprintln!("  - {problem}");
            }
            return ExitCode::from(2);
        }
    };
    let Ok(filter) = tracing_subscriber::EnvFilter::try_new(&config.log_filter) else {
        eprintln!("meetcrm-server: configuration refused:\n  - MEETCRM_LOG is not a valid filter");
        return ExitCode::from(2);
    };
    match config.log_format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_current_span(true)
            .with_env_filter(filter)
            .init(),
        LogFormat::Text => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "could not start the runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(Drained::Clean) => ExitCode::SUCCESS,
        Ok(Drained::DeadlineExceeded { open }) => {
            tracing::warn!(open, "stopped with connections cut at the drain deadline");
            ExitCode::FAILURE
        }
        Err(Stop::Refused(why)) => {
            tracing::error!(reason = %why, "refusing to run");
            ExitCode::from(3)
        }
        Err(Stop::Schema(status)) => {
            tracing::error!(schema = %status, "refusing to run: the schema is not this binary's; see docs/OPERATIONS.md");
            ExitCode::from(4)
        }
        Err(Stop::Failed(e)) => {
            tracing::error!(error = %format!("{e:#}"), "stopped");
            ExitCode::FAILURE
        }
    }
}

enum Stop {
    Refused(String),
    Schema(String),
    Failed(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Stop {
    fn from(e: E) -> Self {
        Self::Failed(e.into())
    }
}

async fn run(config: Config) -> Result<Drained, Stop> {
    let (shutdown, signal) = meetcrm_http::shutdown::on_signal()?;
    let pool = meetcrm_db::connect_lazy(config.database_url.expose(), &config.db)?;
    // The role first; then the schema, since another release's schema also changes what
    // the manifest sees (a newer table, a missing one) and is the cause to report; then the
    // protections.
    let guarded = check_at_startup(&pool, config.startup_timeout).await;
    if let Err(Refusal::Db(e)) = &guarded {
        return Err(Stop::Failed(anyhow::anyhow!(meetcrm_db::chain(e))));
    }
    if matches!(guarded, Ok(()) | Err(Refusal::Unprotected(_))) {
        let status = schema::status(&pool, &meetcrm_db::MIGRATOR).await?;
        if !status.is_current() {
            return Err(Stop::Schema(serde_json::to_string(&status)?));
        }
    }
    if let Err(refused) = guarded {
        return Err(Stop::Refused(refused.to_string()));
    }
    let mail: Arc<dyn MailTransport> = if let Some(relay) = config.smtp_relay {
        Arc::new(SmtpRelay::new(relay)?)
    } else {
        tracing::warn!(
            "MEETCRM_SMTP_RELAY is empty: mail is disabled, sign-up reports verification email failed"
        );
        Arc::new(Disabled)
    };
    let accounts = Accounts::new(pool.clone(), config.accounts.clone(), mail).await?;
    let errors = Arc::new(ErrorWindow::default());
    let health = Health::new(
        pool.clone(),
        config.ready_timeout,
        errors.clone(),
        config.error_rate_permille,
        config.error_min_requests,
    );
    let media = &config.media;
    let transcriber: Arc<dyn Transcriber> = match &media.transcriber {
        TranscriberConfig::Recorded(dir) => Arc::new(Recorded::new(dir)),
        TranscriberConfig::Hosted { url, key } => Arc::new(Hosted::new(
            url,
            key.expose(),
            media.meetings.provider_deadline,
        )?),
    };
    let meetings = Meetings::new(
        pool.clone(),
        accounts.clone(),
        FileStore::open(&media.dir).await?,
        Tools {
            ffmpeg: media.ffmpeg_dir.join("ffmpeg"),
            ffprobe: media.ffmpeg_dir.join("ffprobe"),
            prlimit: media.prlimit.clone(),
        },
        transcriber,
        media.meetings.clone(),
    );
    let workers = tokio::spawn(meetings.clone().run_workers(shutdown.clone()));
    let router = app(AppState {
        accounts,
        health,
        errors,
        stack: config.stack,
        max_in_flight: config.max_in_flight,
        meetings,
        upload: media.upload,
    });
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(listen = %listener.local_addr()?, version = env!("CARGO_PKG_VERSION"), "listening");
    let drained = serve(listener, router, config.connection, shutdown).await;
    if let Ok(name) = signal.await {
        tracing::info!(signal = name, "drained after signal");
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), workers).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pool.close()).await;
    Ok(drained)
}
