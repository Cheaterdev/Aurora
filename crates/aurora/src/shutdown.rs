//! Ordered async callbacks run during shutdown, after the HTTP server has
//! drained and before the store closes (so they can still write). TRK-45
//! registers the "mark active runs interrupted" hook here.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::supervise::panic_message;

/// A hook that has not finished by then is abandoned so shutdown always ends.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

type HookFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type Hook = Box<dyn FnOnce() -> HookFuture + Send>;

#[derive(Default)]
pub struct ShutdownHooks {
    hooks: Vec<(&'static str, Hook)>,
}

impl ShutdownHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hooks run one at a time in registration order. A hook that fails,
    /// panics or times out is logged and the remaining hooks still run.
    pub fn register<F, Fut>(&mut self, name: &'static str, hook: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.hooks.push((name, Box::new(move || Box::pin(hook()))));
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub async fn run(self) {
        for (name, hook) in self.hooks {
            tracing::info!(hook = name, "running shutdown hook");
            let mut task = tokio::spawn(hook());
            match tokio::time::timeout(HOOK_TIMEOUT, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(err))) => {
                    tracing::error!(hook = name, error = %format!("{err:#}"), "shutdown hook failed");
                }
                Ok(Err(join)) if join.is_panic() => {
                    let message = panic_message(join.into_panic().as_ref());
                    tracing::error!(hook = name, panic = %message, "shutdown hook panicked");
                }
                Ok(Err(_)) => tracing::warn!(hook = name, "shutdown hook was cancelled"),
                Err(_) => {
                    task.abort();
                    tracing::error!(hook = name, "shutdown hook timed out and was abandoned");
                }
            }
        }
    }
}
