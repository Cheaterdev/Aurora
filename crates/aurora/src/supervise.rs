//! Background tasks must not fail silently: a panic or an error in a spawned
//! task is logged with its name instead of being lost with the join handle.

use std::any::Any;
use std::future::Future;

use tokio::task::JoinHandle;

/// Best-effort text of a panic payload (`panic!` with a literal or a format).
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Spawns `task` and logs its error or panic under `name`. The returned handle
/// resolves once the task has ended and the outcome has been logged; it never
/// yields an error and never panics.
pub fn spawn_supervised<F>(name: &'static str, task: F) -> JoinHandle<()>
where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let inner = tokio::spawn(task);
    tokio::spawn(async move {
        match inner.await {
            Ok(Ok(())) => tracing::debug!(task = name, "background task finished"),
            Ok(Err(err)) => {
                tracing::error!(task = name, error = %format!("{err:#}"), "background task failed")
            }
            Err(join) if join.is_panic() => {
                let message = panic_message(join.into_panic().as_ref());
                tracing::error!(task = name, panic = %message, "background task panicked");
            }
            Err(_) => tracing::warn!(task = name, "background task was cancelled"),
        }
    })
}
