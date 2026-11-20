//! SIGINT (Ctrl+C) and SIGTERM (service managers, `kill`).

use std::future::Future;

use tokio::signal::unix::{SignalKind, signal};

macro_rules! wait_for {
    ($kind:expr, $name:literal) => {{
        let registered = signal($kind);
        async move {
            match registered {
                Ok(mut signal) => {
                    signal.recv().await;
                    $name
                }
                Err(err) => {
                    tracing::error!(%err, signal = $name, "cannot listen for signal");
                    std::future::pending::<&'static str>().await
                }
            }
        }
    }};
}

/// Registers the handlers now (so no signal between this call and the first
/// poll is lost) and resolves with the name of the first signal received.
pub fn shutdown_signal() -> impl Future<Output = &'static str> + Send + 'static {
    let int = wait_for!(SignalKind::interrupt(), "SIGINT");
    let term = wait_for!(SignalKind::terminate(), "SIGTERM");
    async move {
        tokio::select! {
            name = int => name,
            name = term => name,
        }
    }
}
