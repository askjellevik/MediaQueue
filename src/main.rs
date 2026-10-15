#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::{
    io::{Read, Write},
    net::TcpStream,
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};
use mediaqueue::{auth, build_router, config::Config, db, state::AppState, tasks};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "mediaqueue", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve,
    /// Check that the local server answers `/healthz`; exits 0 or 1.
    Healthcheck,
    /// Prompt for a password (no echo) and print its Argon2id PHC hash,
    /// for `ADMIN_PASSWORD_HASH`.
    HashPassword,
    /// Create a member. Without `--code` a random access code is generated
    /// and printed once; it cannot be recovered, so rotate it if it's lost.
    AddMember {
        #[arg(long)]
        name: String,
        /// Use this access code instead of generating one.
        #[arg(long)]
        code: Option<String>,
        #[arg(long)]
        daily_limit: Option<i64>,
        #[arg(long)]
        max_sessions: Option<i64>,
    },
    /// Write a consistent snapshot of the database to the backups directory
    /// next to it (e.g. `/data/backups`), keeping the newest 14.
    Backup,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => run_serve(),
        Command::Healthcheck => run_healthcheck(),
        Command::HashPassword => run_hash_password(),
        Command::AddMember {
            name,
            code,
            daily_limit,
            max_sessions,
        } => run_add_member(name, code, daily_limit, max_sessions),
        Command::Backup => run_backup(),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
}

fn run_serve() -> ExitCode {
    init_tracing();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };

    warn_if_insecure_origin(&config.public_origin);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve_async(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Plain http is only sensible on the machine itself; elsewhere the session
/// cookies would travel unencrypted.
fn warn_if_insecure_origin(origin: &str) {
    let Some(rest) = origin.strip_prefix("http://") else {
        return;
    };
    let host = rest.split(':').next().unwrap_or(rest);
    if !matches!(host, "localhost" | "127.0.0.1") && !rest.starts_with("[::1]") {
        tracing::warn!(
            origin,
            "PUBLIC_ORIGIN is plain http on a non-local host; cookies are sent without the Secure flag"
        );
    }
}

async fn serve_async(config: Config) -> anyhow::Result<()> {
    let pool = db::connect(&config).await?;
    let bind_addr = config.bind_addr;
    tasks::spawn_session_cleanup(pool.clone());
    tasks::spawn_daily_backup(pool.clone(), config.database_path.clone());
    let state = AppState::new(pool.clone(), config)?;
    tasks::spawn_rate_limit_maintenance(state.limits.clone());
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(addr = %bind_addr, "listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("shutting down, checkpointing WAL");
    db::checkpoint(&pool).await?;
    log_process_stats();

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Plain stdlib HTTP GET to the local server's `/healthz`, for use as the
/// container's `HEALTHCHECK`. No tokio, no shell, no curl in the image.
fn run_healthcheck() -> ExitCode {
    match check_health() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(_) => ExitCode::FAILURE,
    }
}

fn check_health() -> std::io::Result<bool> {
    let mut stream = TcpStream::connect(("127.0.0.1", 8000))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    Ok(text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200"))
}

/// Prompts for a password with no terminal echo and prints its Argon2id
/// PHC hash. Needs no config or database: this is meant to be run before
/// `.env` exists, to produce the value that goes into it.
fn run_hash_password() -> ExitCode {
    let password = match rpassword::prompt_password("Password: ") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to read password: {e}");
            return ExitCode::FAILURE;
        }
    };
    match auth::hash_admin_password(&password) {
        Ok(hash) => {
            println!("{hash}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to hash password: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_add_member(
    name: String,
    code: Option<String>,
    daily_limit: Option<i64>,
    max_sessions: Option<i64>,
) -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(add_member_async(
        config,
        name,
        code,
        daily_limit,
        max_sessions,
    ))
}

async fn add_member_async(
    config: Config,
    name: String,
    chosen: Option<String>,
    daily_limit: Option<i64>,
    max_sessions: Option<i64>,
) -> ExitCode {
    let pool = match db::connect(&config).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("database error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let max_sessions = max_sessions.unwrap_or(mediaqueue::config::DEFAULT_MAX_SESSIONS);
    // A chosen code is validated and hashed; otherwise one is generated and
    // printed once.
    let (shown, normalised) = match chosen {
        Some(chosen) => match auth::check_custom_code(&chosen, config.code_length) {
            Some(normalised) => (None, normalised),
            None => {
                eprintln!(
                    "the code must be {} to {} characters (spaces and hyphens do not count)",
                    config.code_length,
                    auth::MAX_CUSTOM_CODE_LENGTH
                );
                return ExitCode::FAILURE;
            }
        },
        None => match auth::generate_code(config.code_length) {
            Ok(code) => {
                let normalised = auth::normalise_code(&code);
                (Some(code), normalised)
            }
            Err(e) => {
                eprintln!("could not read the OS random source: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let code_hash = auth::hmac_code(&config.code_pepper, &normalised);
    let now = db::now_unix();

    match db::insert_member(&pool, &name, &code_hash, max_sessions, daily_limit, now).await {
        Ok(id) => {
            println!("Member #{id} ({name}) created.");
            if let Some(code) = shown {
                println!(
                    "Access code (shown once, write it down now): {}",
                    auth::format_code_for_display(&code)
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) if matches!(&e, sqlx::Error::Database(d) if d.is_unique_violation()) => {
            eprintln!("failed to create member: that code is already in use");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("failed to create member: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_backup() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let pool = match db::connect(&config).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("database error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let dir = tasks::backup_dir(&config.database_path);
        match tasks::backup_now(&pool, &dir, db::now_unix()).await {
            Ok(path) => {
                println!("Backup written to {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("backup failed: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

/// Logs peak resident memory and total CPU time for the life of the
/// process, read from `/proc/self` (Linux only; silently skipped
/// elsewhere). Used to measure the load test and handy after an incident.
fn log_process_stats() {
    let peak_rss_kb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        });
    // utime and stime are fields 14 and 15, in clock ticks (USER_HZ, 100
    // on Linux). The process name in field 2 may hold spaces, so count
    // from the closing parenthesis.
    let cpu_ms = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let rest = stat.rsplit_once(')')?.1;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            Some((utime + stime) * 10)
        });
    if let (Some(peak_rss_kb), Some(cpu_ms)) = (peak_rss_kb, cpu_ms) {
        tracing::info!(peak_rss_kb, cpu_ms, "process totals");
    }
}
