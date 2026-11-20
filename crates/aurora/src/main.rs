use anyhow::{Context, Result};
use aurora::config::{Command, Config, USAGE};
use aurora_store::Store;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = match Config::parse(std::env::args().skip(1), |k| std::env::var(k).ok())? {
        Command::Run(config) => config,
        Command::Help => {
            println!("{USAGE}");
            return Ok(());
        }
    };

    let store = Store::open(&config.db_path)
        .with_context(|| format!("opening database {}", config.db_path.display()))?;
    tracing::info!(
        path = %config.db_path.display(),
        schema_version = store.schema_version(),
        "database ready"
    );

    let listener = TcpListener::bind((config.host, config.port))
        .await
        .with_context(|| format!("binding {}:{}", config.host, config.port))?;
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
