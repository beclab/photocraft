#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use photocraft_agent_server::{Config, router};

const USAGE: &str = "\
photocraft-agent-server: run PhotoCraft engine commands on uploaded images over HTTP

Environment:
  PHOTOCRAFT_AGENT_ADDR            listen address (default 0.0.0.0:8090)
  PHOTOCRAFT_AGENT_MAX_UPLOAD_MB   request body limit in MiB (default 256)
  PHOTOCRAFT_AGENT_MAX_JOBS        jobs that run at once (default 2)
  PHOTOCRAFT_AGENT_TIMEOUT_SECS    per-request limit in seconds (default 300)

Endpoints: GET /healthz, GET /v1/commands?filter=, POST /v1/process, POST /v1/inspect
";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("photocraft-agent-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("photocraft-agent-server: unknown argument `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
        None => {}
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("photocraft-agent-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn env_number<T: FromStr>(name: &str, default: T) -> Result<T, String> {
    match std::env::var(name) {
        Ok(raw) => raw.trim().parse().map_err(|_| format!("{name} must be a non-negative integer, got `{raw}`")),
        Err(_) => Ok(default),
    }
}

fn run() -> Result<(), String> {
    let addr = std::env::var("PHOTOCRAFT_AGENT_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let cfg = Config {
        max_upload_bytes: env_number::<usize>("PHOTOCRAFT_AGENT_MAX_UPLOAD_MB", 256)?.saturating_mul(1 << 20),
        max_jobs: env_number("PHOTOCRAFT_AGENT_MAX_JOBS", 2)?,
        timeout: Duration::from_secs(env_number("PHOTOCRAFT_AGENT_TIMEOUT_SECS", 300)?),
    };
    // The CLI and desktop app run the engine on a main thread (8 MiB on Linux); the runtime's
    // blocking threads default to 2 MiB.
    let runtime =
        tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(8 << 20).build().map_err(|e| format!("cannot start the runtime: {e}"))?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        eprintln!("photocraft-agent-server {} listening on {addr}", env!("CARGO_PKG_VERSION"));
        axum::serve(listener, router(&cfg)).with_graceful_shutdown(shutdown_signal()).await.map_err(|e| format!("server error: {e}"))
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
