//! The Aurora server: configuration and the HTTP surface.

pub mod config;

use std::future::Future;

use anyhow::{Context, Result};
use aurora_store::Store;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    schema_version: i64,
    version: &'static str,
}

async fn health(State(store): State<Store>) -> Json<Health> {
    Json(Health {
        status: "ok",
        schema_version: store.schema_version(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(store)
}

/// Serves until `shutdown` resolves, then lets in-flight requests finish and
/// closes the store so the writer drains its queue.
pub async fn serve(
    listener: TcpListener,
    store: Store,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let result = axum::serve(listener, router(store.clone()))
        .with_graceful_shutdown(shutdown)
        .await
        .context("http server failed");
    store.close().await;
    result
}
