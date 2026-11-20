//! The Aurora server: configuration and the HTTP surface.

pub mod auth;
pub mod config;
pub mod routes;
pub mod shutdown;
#[cfg_attr(windows, path = "signal/windows.rs")]
#[cfg_attr(not(windows), path = "signal/other.rs")]
mod signal;
pub mod supervise;

use std::any::Any;
use std::future::{Future, IntoFuture};
use std::time::Duration;

use anyhow::{Context, Result};
use aurora_store::Store;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::catch_panic::CatchPanicLayer;

pub use shutdown::ShutdownHooks;
pub use signal::shutdown_signal;
pub use supervise::spawn_supervised;

/// How long in-flight requests get to finish once shutdown starts; after that
/// the remaining connections are abandoned and shutdown continues.
pub const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

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

fn app_routes(store: Store) -> Router<Store> {
    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", routes::api_router(store))
}

fn panic_response(payload: Box<dyn Any + Send + 'static>) -> Response {
    let message = supervise::panic_message(payload.as_ref());
    tracing::error!(panic = %message, "request handler panicked");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"internal server error"}"#,
    )
        .into_response()
}

/// Layers every router shares; a handler panic becomes a 500 for that request only.
fn with_layers(router: Router) -> Router {
    router.layer(CatchPanicLayer::custom(panic_response))
}

pub fn router(store: Store) -> Router {
    with_layers(app_routes(store.clone()).with_state(store))
}

/// Serves until `shutdown` resolves, then stops accepting connections, lets
/// in-flight requests finish (up to [`HTTP_DRAIN_TIMEOUT`]), runs `hooks` and
/// closes the store so the writer drains its queue and checkpoints the WAL.
pub async fn serve(
    listener: TcpListener,
    store: Store,
    shutdown: impl Future<Output = ()> + Send + 'static,
    hooks: ShutdownHooks,
) -> Result<()> {
    let app = router(store.clone());
    serve_router(listener, app, store, shutdown, hooks, HTTP_DRAIN_TIMEOUT).await
}

async fn serve_router(
    listener: TcpListener,
    app: Router,
    store: Store,
    shutdown: impl Future<Output = ()> + Send + 'static,
    hooks: ShutdownHooks,
    drain_timeout: Duration,
) -> Result<()> {
    let (draining, drain_started) = oneshot::channel::<()>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            tracing::info!("draining in-flight requests");
            let _ = draining.send(());
        })
        .into_future();
    tokio::pin!(server);
    let deadline = async {
        if drain_started.await.is_ok() {
            tokio::time::sleep(drain_timeout).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    let result = tokio::select! {
        served = &mut server => served.context("http server failed"),
        () = deadline => {
            tracing::warn!(
                timeout_secs = drain_timeout.as_secs_f32(),
                "in-flight requests did not finish in time; dropping them"
            );
            Ok(())
        }
    };
    tracing::info!("http server stopped");

    if !hooks.is_empty() {
        tracing::info!(count = hooks.len(), "running shutdown hooks");
        hooks.run().await;
    }
    tracing::info!("closing the store");
    store.close().await;
    tracing::info!("shutdown complete");
    result
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    async fn get_raw(addr: std::net::SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        raw
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogBuffer {
        fn text(&self) -> String {
            let bytes = self.0.lock().unwrap_or_else(|p| p.into_inner()).clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// Thread-local, so it sees everything the current-thread test runtime runs.
    fn capture_logs() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (buffer, tracing::subscriber::set_default(subscriber))
    }

    /// Exists only in test builds; the release binary has no panic route.
    fn test_app(store: Store) -> Router {
        let routes = app_routes(store.clone())
            .route(
                "/__test/panic",
                get(|| async { panic!("handler exploded") }),
            )
            .route(
                "/__test/hang",
                get(|| async {
                    std::future::pending::<()>().await;
                }),
            );
        with_layers(routes.with_state(store))
    }

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn panicking_handler_returns_500_and_server_keeps_serving() {
        let (logs, _guard) = capture_logs();
        let (_dir, store) = temp_store();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_router(
            listener,
            test_app(store.clone()),
            store,
            async {
                let _ = stopped.await;
            },
            ShutdownHooks::new(),
            HTTP_DRAIN_TIMEOUT,
        ));

        let raw = get_raw(addr, "/__test/panic").await;
        assert!(raw.starts_with("HTTP/1.1 500"), "unexpected: {raw}");
        assert!(raw.contains(r#"{"error":"internal server error"}"#));
        assert!(logs.text().contains("handler exploded"));

        for _ in 0..2 {
            let raw = get_raw(addr, "/health").await;
            assert!(raw.starts_with("HTTP/1.1 200"), "unexpected: {raw}");
        }

        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn panicking_supervised_task_is_logged_and_process_survives() {
        let (logs, _guard) = capture_logs();
        spawn_supervised("explodes", async {
            panic!("task exploded");
        })
        .await
        .unwrap();
        spawn_supervised("fails", async { anyhow::bail!("task failed") })
            .await
            .unwrap();
        spawn_supervised("fine", async { Ok(()) }).await.unwrap();

        let text = logs.text();
        assert!(text.contains("background task panicked"), "{text}");
        assert!(text.contains("task exploded"), "{text}");
        assert!(text.contains("task failed"), "{text}");
    }

    #[tokio::test]
    async fn hooks_run_in_order_after_the_server_stops_and_before_the_store_closes() {
        let (_dir, store) = temp_store();
        let marker = aurora_store::marker_path(&store_path(&_dir));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let events = Arc::new(Mutex::new(Vec::<String>::new()));

        let mut hooks = ShutdownHooks::new();
        for name in ["first", "second", "third"] {
            let events = events.clone();
            let marker = marker.clone();
            let store = store.clone();
            hooks.register(name, move || async move {
                // The server is already down, the store can still write.
                // Windows takes seconds to report a refused connection.
                let connected =
                    tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr))
                        .await;
                let refused = !matches!(connected, Ok(Ok(_)));
                store
                    .write(move |tx| {
                        tx.execute(
                            "INSERT INTO project (key, name, created) VALUES (?1, 'P', 1)",
                            [name],
                        )?;
                        Ok(())
                    })
                    .await?;
                events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(format!(
                        "{name} refused={refused} marker={}",
                        marker.exists()
                    ));
                if name == "second" {
                    anyhow::bail!("a failing hook must not stop the rest");
                }
                Ok(())
            });
        }

        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            store.clone(),
            async {
                let _ = stopped.await;
            },
            hooks,
        ));
        assert!(get_raw(addr, "/health").await.starts_with("HTTP/1.1 200"));
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();

        let events = events.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(
            events,
            [
                "first refused=true marker=true",
                "second refused=true marker=true",
                "third refused=true marker=true",
            ]
        );
        assert!(!marker.exists(), "the store closed after the hooks");
    }

    fn store_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("test.db")
    }

    #[tokio::test]
    async fn stuck_request_is_dropped_after_the_drain_timeout() {
        let (_dir, store) = temp_store();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_router(
            listener,
            test_app(store.clone()),
            store,
            async {
                let _ = stopped.await;
            },
            ShutdownHooks::new(),
            Duration::from_millis(300),
        ));

        let mut stuck = TcpStream::connect(addr).await.unwrap();
        stuck
            .write_all(b"GET /__test/hang HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown must not wait for the stuck request")
            .unwrap()
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
    }
}
