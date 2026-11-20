//! Console control events. Ctrl+Break is included because a child started in
//! its own process group (as the service wrapper and the tests do) never
//! receives Ctrl+C.

use std::future::Future;

use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

macro_rules! wait_for {
    ($registered:expr, $name:literal) => {{
        let registered = $registered;
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
/// poll is lost) and resolves with the name of the first event received.
pub fn shutdown_signal() -> impl Future<Output = &'static str> + Send + 'static {
    let c = wait_for!(ctrl_c(), "Ctrl+C");
    let b = wait_for!(ctrl_break(), "Ctrl+Break");
    let close = wait_for!(ctrl_close(), "console close");
    let shutdown = wait_for!(ctrl_shutdown(), "system shutdown");
    async move {
        tokio::select! {
            name = c => name,
            name = b => name,
            name = close => name,
            name = shutdown => name,
        }
    }
}
