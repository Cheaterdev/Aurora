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

    let command = match Config::parse(std::env::args().skip(1), |k| std::env::var(k).ok()) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("aurora: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let config = match command {
        Command::Run(config) => config,
        Command::Check(check) => return aurora::check::run(&check),
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("aurora: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<()> {
    let options = OpenOptions {
        backup_dir: config.backup_dir.clone(),
        allow_unsafe_location: config.allow_unsafe_location,
        request_retention: config.request_retention,
        public_url: config.public_url.clone(),
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

    // Registered before the server starts answering so an early signal is not lost.
    let signal = aurora::shutdown_signal();
    let listener = match TcpListener::bind((config.host, config.port)).await {
        Ok(listener) => listener,
        Err(err) => {
            store.close().await;
            return Err(err).with_context(|| format!("binding {}:{}", config.host, config.port));
        }
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");

    // Reconciles runs a crash left active, then starts the dispatcher; its
    // shutdown (kill children, mark runs interrupted) is a hook of `serve_with`.
    // TRK-45 hardens this (stray processes, moving cards).
    let runner_config = aurora::runner::RunnerConfig::for_server(
        &store,
        listener.local_addr()?,
        aurora::runner::RunnerOptions::default(),
    );
    let runner = match aurora::Runner::start(store.clone(), runner_config).await {
        Ok(runner) => runner,
        Err(err) => {
            store.close().await;
            return Err(err).context("starting the runner");
        }
    };
    let hooks = aurora::ShutdownHooks::new();

    aurora::serve_with(
        listener,
        store,
        async move {
            let name = signal.await;
            tracing::info!(signal = name, "shutdown signal received");
        },
        hooks,
        Some(runner),
    )
    .await
}
