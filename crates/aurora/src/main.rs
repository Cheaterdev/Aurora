use std::process::ExitCode;

use anyhow::{Context, Result};
use aurora::config::{Command, Config, USAGE};
use aurora_store::{OpenOptions, Store};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("aurora: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let config = match Config::parse(std::env::args().skip(1), |k| std::env::var(k).ok())? {
        Command::Run(config) => config,
        Command::Help => {
            println!("{USAGE}");
            return Ok(());
        }
    };

    let options = OpenOptions {
        backup_dir: config.backup_dir.clone(),
        allow_unsafe_location: config.allow_unsafe_location,
    };
    let store = Store::open_with(&config.db_path, &options)?;
    let report = store.open_report();
    tracing::info!(
        path = %config.db_path.display(),
        schema_version = store.schema_version(),
        integrity_check = report.integrity_check.name(),
        unclean_shutdown = report.unclean_shutdown,
        backup_dir = %report.backup_dir.display(),
        "database ready"
    );

    let listener = match TcpListener::bind((config.host, config.port)).await {
        Ok(listener) => listener,
        Err(err) => {
            store.close().await;
            return Err(err).with_context(|| format!("binding {}:{}", config.host, config.port));
        }
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");

    aurora::serve(listener, store, async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "cannot listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
        tracing::info!("shutting down");
    })
    .await
}
