//! Live updates: `GET /events/stream`, a server-sent-events view of the event log.
//!
//! One poller task reads the newest event id every [`POLL_EVERY`] (only while
//! a stream is open) and publishes it on a watch channel; each open stream
//! wakes on a change, reads the events after its own cursor on a read-only
//! connection and sends them. Nothing here touches the writer. A stream ends
//! when shutdown starts, so the HTTP drain does not wait for it.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use aurora_store::events::{self, Event as LogEvent, EventFilter};
use aurora_store::failpoints::{self, Point};
use aurora_store::projects::get_project;
use aurora_store::{Store, StoreError};
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::routes::{ApiError, ApiQuery};

pub const POLL_EVERY: Duration = Duration::from_millis(500);
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
const BATCH: i64 = 100;

/// What handlers hold: the shutdown flag and the newest event id.
#[derive(Clone)]
pub struct Live {
    stop: watch::Receiver<bool>,
    latest: watch::Receiver<i64>,
}

/// Held by the server; [`LiveControl::stop`] ends every open stream.
pub struct LiveControl {
    stop: watch::Sender<bool>,
}

impl LiveControl {
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl Live {
    /// Starts the poller on the current runtime (if there is one).
    pub fn start(store: Store) -> (Live, LiveControl) {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (latest_tx, latest_rx) = watch::channel(0_i64);
        let live = Live {
            stop: stop_rx,
            latest: latest_rx,
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(poll_latest(store, latest_tx, live.stop.clone()));
        }
        (live, LiveControl { stop: stop_tx })
    }

    fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }

    async fn stopped(&self) {
        wait_stopped(&self.stop).await;
    }
}

/// Resolves when shutdown starts; never if the server dropped its control.
async fn wait_stopped(stop: &watch::Receiver<bool>) {
    let mut stop = stop.clone();
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn poll_latest(store: Store, latest: watch::Sender<i64>, stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(POLL_EVERY) => {}
            () = wait_stopped(&stop) => return,
        }
        if latest.is_closed() {
            return;
        }
        // The router's own handle is one receiver; each open stream adds one.
        if latest.receiver_count() <= 1 {
            continue;
        }
        match store.read(events::latest_event_id).await {
            Ok(id) => {
                latest.send_if_modified(|current| {
                    let changed = *current != id;
                    *current = id;
                    changed
                });
            }
            Err(err) => tracing::warn!(error = %err, "could not read the newest event id"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamQuery {
    /// Start after this event id (the page passes the id it was rendered at).
    after: Option<i64>,
    /// Only events of this project.
    project: Option<String>,
}

struct Tail {
    store: Store,
    live: Live,
    changed: watch::Receiver<i64>,
    after: i64,
    filter: EventFilter,
    queue: VecDeque<LogEvent>,
}

impl Tail {
    /// The next event to send, or `None` when the stream should end.
    async fn next_event(&mut self) -> Option<LogEvent> {
        loop {
            if self.live.is_stopped() {
                return None;
            }
            if let Some(event) = self.queue.pop_front() {
                self.after = event.id;
                return Some(event);
            }
            // Marked seen before reading, so a change during the read wakes us again.
            self.changed.borrow_and_update();
            let after = self.after;
            let filter = self.filter.clone();
            match self
                .store
                .read(move |c| events::read_events(c, after, Some(BATCH), &filter))
                .await
            {
                Ok(list) if !list.is_empty() => {
                    failpoints::hit(Point::BeforeEventDelivery);
                    self.queue = list.into();
                    continue;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "event stream ends: could not read events");
                    return None;
                }
            }
            tokio::select! {
                () = self.live.stopped() => return None,
                changed = self.changed.changed() => {
                    if changed.is_err() {
                        // The poller is gone; do not spin.
                        tokio::time::sleep(POLL_EVERY).await;
                    }
                }
            }
        }
    }
}

fn sse_event(event: &LogEvent) -> Event {
    // No payload: it can be large, and clients re-fetch what they show.
    let data = json!({
        "id": event.id,
        "project": event.project,
        "kind": event.kind,
        "entity_kind": event.entity_kind,
        "entity_id": event.entity_id,
        "actor": event.actor,
        "created": event.created,
    });
    Event::default()
        .id(event.id.to_string())
        .data(data.to_string())
}

fn last_event_id(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Starts after `Last-Event-ID` when the browser reconnects, else after
/// `?after=`, else after the newest event now.
pub async fn events_stream(
    State(store): State<Store>,
    Extension(live): Extension<Live>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<StreamQuery>,
) -> Result<Response, ApiError> {
    let requested = last_event_id(&headers).or(query.after);
    let filter = EventFilter {
        project: query.project,
        ..EventFilter::default()
    };
    let checked = filter.clone();
    let after = store
        .read(move |c| -> Result<i64, StoreError> {
            if let Some(key) = &checked.project {
                get_project(c, key)?;
            }
            let latest = events::latest_event_id(c)?;
            Ok(requested.map_or(latest, |id| id.clamp(0, latest)))
        })
        .await?;

    let tail = Tail {
        changed: live.latest.clone(),
        store,
        live,
        after,
        filter,
        queue: VecDeque::new(),
    };
    let events = stream::unfold(tail, |mut tail| async move {
        let event = tail.next_event().await?;
        Some((Ok::<Event, Infallible>(sse_event(&event)), tail))
    });
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive"))
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
