//! Kill-loop soak: runs the real `aurora` binary under a concurrent mixed
//! workload and hard-kills it every 2 to 10 seconds, restarting it each time.
//! After every restart it runs the invariant checks (the library and the
//! `aurora check` command), proves that every write the clients saw
//! acknowledged is still there, and that retries with the same
//! `Idempotency-Key` never duplicated anything.
//!
//! One more thread starts runs of a fake agent with an overall limit of two
//! (`AURORA_MAX_RUNS=2`), faster than they finish, cancels some of what waits,
//! and the checks assert the limit, the queue order, and that nothing is
//! promoted or finished twice. Build the example agent first with
//! `cargo build --examples -p aurora`.
//!
//! Ignored by default. Run it with
//!
//! ```text
//! AURORA_SOAK_SECONDS=3600 cargo test --test soak -- --ignored --nocapture
//! ```
//!
//! (default 120 seconds). Only this test's own child process is ever killed.
//! The test fails, after printing a summary, if any violation was seen.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

mod common;
use common::harness::{Resp, Server, assert_checks_green, aurora_check, bootstrap, count, request};

const WORKERS: u64 = 4;
const PROJECT: &str = "SOAK";
/// Entries kept to re-verify on later restarts (immutable facts only).
const RESERVOIR: usize = 2000;
const RESAMPLE: usize = 200;
const OWN_ISSUES: usize = 200;
const COMMAND_CHECK_EVERY: u64 = 5;

/// Something a client was told succeeded.
#[derive(Clone, Debug)]
enum Entry {
    Issue {
        number: i64,
        title: String,
    },
    Comment {
        id: i64,
        body: String,
    },
    Link {
        from: i64,
        to: i64,
    },
    Doc {
        id: i64,
        title: String,
    },
    /// Mutable facts: checked once, on the restart after they were acknowledged.
    Column {
        number: i64,
        column: i64,
    },
    Archived {
        number: i64,
    },
    DocBody {
        id: i64,
        body: String,
    },
}

impl Entry {
    fn immutable(&self) -> bool {
        matches!(
            self,
            Entry::Issue { .. } | Entry::Comment { .. } | Entry::Link { .. } | Entry::Doc { .. }
        )
    }

    /// Later entries for the same mutable fact replace earlier ones.
    fn slot(&self) -> Option<(&'static str, i64)> {
        match self {
            Entry::Column { number, .. } => Some(("column", *number)),
            Entry::Archived { number } => Some(("archived", *number)),
            Entry::DocBody { id, .. } => Some(("doc-body", *id)),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Ledger {
    pending: Vec<Entry>,
    reservoir: Vec<Entry>,
    seen: u64,
}

struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Rng {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Rng(nanos ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

struct Shared {
    addr: Mutex<Option<String>>,
    key: String,
    stop: AtomicBool,
    ledger: Mutex<Ledger>,
    violations: Mutex<Vec<String>>,
    acked: AtomicU64,
    creates_acked: AtomicU64,
    creates_in_flight: AtomicI64,
    resends: AtomicU64,
    replays: AtomicU64,
    duplicate_probes: AtomicU64,
    consumer_acked: AtomicI64,
    runs_started: AtomicU64,
    runs_refused: AtomicU64,
    columns: Mutex<Vec<i64>>,
    type_id: AtomicI64,
    /// Moves sent and not yet answered, per issue. A move the server committed
    /// just before it was killed, or whose answer is not recorded yet, may
    /// legitimately be what the database shows.
    moving: Mutex<BTreeMap<i64, Vec<i64>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn violation(&self, message: String) {
        eprintln!("VIOLATION: {message}");
        locked(&self.violations).push(message);
        self.stop.store(true, Ordering::SeqCst);
    }

    fn set_addr(&self, addr: Option<String>) {
        *locked(&self.addr) = addr;
    }

    fn record(&self, entry: Entry) {
        locked(&self.ledger).pending.push(entry);
    }

    /// Sends the request, and when the server does not answer (it was killed)
    /// waits for the next one and sends the very same request, with the same
    /// `Idempotency-Key`, again. `None` only when the soak is ending.
    fn send(
        &self,
        method: &'static str,
        path: &str,
        body: Option<&Value>,
        idem: Option<&str>,
    ) -> Option<Resp> {
        let mut resent = false;
        loop {
            let addr = locked(&self.addr).clone();
            let Some(addr) = addr else {
                if self.stopped() {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            match request(&addr, &self.key, method, path, body, idem) {
                Ok(resp) => {
                    if resp.status >= 500 {
                        self.violation(format!(
                            "{method} {path} answered {}: {}",
                            resp.status, resp.body
                        ));
                    }
                    if resent && resp.replayed() {
                        self.replays.fetch_add(1, Ordering::SeqCst);
                    }
                    if idem.is_some() && resp.is_success() {
                        self.acked.fetch_add(1, Ordering::SeqCst);
                    }
                    return Some(resp);
                }
                Err(_) => {
                    resent = true;
                    self.resends.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

struct Own {
    number: i64,
}

fn worker(shared: &Shared, id: u64, run: u64) {
    let mut rng = Rng::new(id + 1);
    let mut seq = 0_u64;
    let mut own: Vec<Own> = Vec::new();
    while !shared.stopped() {
        seq += 1;
        let idem = format!("soak-{run}-{id}-{seq}");
        let columns = locked(&shared.columns).clone();
        let op = rng.below(100);
        if own.len() < 3 || op < 35 {
            let title = format!("soak-i-{id}-{seq}");
            let body = json!({
                "title": title,
                "type_id": shared.type_id.load(Ordering::SeqCst),
                "description": format!("created by worker {id}"),
            });
            shared.creates_in_flight.fetch_add(1, Ordering::SeqCst);
            let resp = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/issues"),
                Some(&body),
                Some(&idem),
            );
            shared.creates_in_flight.fetch_sub(1, Ordering::SeqCst);
            let Some(resp) = resp else { return };
            if resp.status != 201 {
                shared.violation(format!(
                    "create issue answered {}: {}",
                    resp.status, resp.body
                ));
                return;
            }
            shared.creates_acked.fetch_add(1, Ordering::SeqCst);
            let number = resp.json()["number"].as_i64().unwrap_or(0);
            shared.record(Entry::Issue {
                number,
                title: title.clone(),
            });
            if own.len() >= OWN_ISSUES {
                own.remove(0);
            }
            own.push(Own { number });
            if rng.below(100) < 15 {
                // The same request again, right away: it must be a replay.
                shared.duplicate_probes.fetch_add(1, Ordering::SeqCst);
                let Some(again) = shared.send(
                    "POST",
                    &format!("/api/v1/projects/{PROJECT}/issues"),
                    Some(&body),
                    Some(&idem),
                ) else {
                    return;
                };
                if again.status != 201
                    || !again.replayed()
                    || again.json()["number"].as_i64() != Some(number)
                {
                    shared.violation(format!(
                        "a repeat of {idem} was not a replay of issue {number}: {} {}",
                        again.status, again.body
                    ));
                    return;
                }
            }
        } else if op < 60 {
            let target = own[rng.below(own.len() as u64) as usize].number;
            let text = format!("soak-c-{id}-{seq}");
            let Some(resp) = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/issues/{target}/comments"),
                Some(&json!({"body": text})),
                Some(&idem),
            ) else {
                return;
            };
            if resp.status == 201 {
                let comment = resp.json()["id"].as_i64().unwrap_or(0);
                shared.record(Entry::Comment {
                    id: comment,
                    body: text,
                });
            }
        } else if op < 78 && !columns.is_empty() {
            let target = own[rng.below(own.len() as u64) as usize].number;
            let column = columns[rng.below(columns.len() as u64) as usize];
            locked(&shared.moving)
                .entry(target)
                .or_default()
                .push(column);
            let Some(resp) = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/issues/{target}/move"),
                Some(&json!({"column_id": column})),
                Some(&idem),
            ) else {
                return;
            };
            if resp.is_success() {
                shared.record(Entry::Column {
                    number: target,
                    column,
                });
            }
            // After record(): a verification that missed the entry still sees the move.
            if let Some(list) = locked(&shared.moving).get_mut(&target)
                && let Some(at) = list.iter().position(|c| *c == column)
            {
                list.remove(at);
            }
        } else if op < 88 {
            let from = own[rng.below(own.len() as u64) as usize].number;
            let to = own[rng.below(own.len() as u64) as usize].number;
            let Some(resp) = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/issues/{from}/links"),
                Some(&json!({"kind": "relates", "to": to})),
                Some(&idem),
            ) else {
                return;
            };
            if resp.status == 201 {
                shared.record(Entry::Link { from, to });
            }
        } else if op < 95 {
            let title = format!("soak-d-{id}-{seq}");
            let Some(resp) = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/documents"),
                Some(&json!({"title": title, "body": "first"})),
                Some(&idem),
            ) else {
                return;
            };
            if resp.status != 201 {
                continue;
            }
            let doc = resp.json()["id"].as_i64().unwrap_or(0);
            shared.record(Entry::Doc { id: doc, title });
            let text = format!("second {id} {seq}");
            let Some(patched) = shared.send(
                "PATCH",
                &format!("/api/v1/projects/{PROJECT}/documents/{doc}"),
                Some(&json!({"body": text, "base_revision": 1})),
                Some(&format!("{idem}-patch")),
            ) else {
                return;
            };
            if patched.is_success() {
                shared.record(Entry::DocBody {
                    id: doc,
                    body: text,
                });
            }
        } else {
            let at = rng.below(own.len() as u64) as usize;
            let target = own.remove(at).number;
            let Some(resp) = shared.send(
                "POST",
                &format!("/api/v1/projects/{PROJECT}/issues/{target}/archive"),
                None,
                Some(&idem),
            ) else {
                return;
            };
            if resp.is_success() {
                shared.record(Entry::Archived { number: target });
            }
        }
        std::thread::sleep(Duration::from_millis(rng.below(15)));
    }
}

/// The overall run limit the soak server runs with.
const MAX_RUNS: i64 = 2;
const SERVER_ENV: &[(&str, &str)] = &[("AURORA_MAX_RUNS", "2")];

/// Pids of agents the soak's servers started that are still the same live
/// process (same recorded creation time). Never matches by name, so it cannot
/// be fooled by (or blame) anybody else's `fake_agent`.
fn stray_agents(db: &Path) -> Vec<i64> {
    let conn = aurora_store::rusqlite::Connection::open_with_flags(
        db,
        aurora_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open read-only");
    let mut stmt = conn
        .prepare("SELECT pid, pid_started_at FROM run WHERE pid IS NOT NULL AND pid_started_at IS NOT NULL")
        .expect("prepare");
    let rows: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .filter_map(Result::ok)
        .collect();
    rows.into_iter()
        .filter(|(pid, started)| {
            u32::try_from(*pid)
                .ok()
                .and_then(aurora::runner::process_identity)
                == Some(*started)
        })
        .map(|(pid, _)| pid)
        .collect()
}

fn fake_agent() -> std::path::PathBuf {
    let name = format!("fake_agent{}", std::env::consts::EXE_SUFFIX);
    let path = std::path::Path::new(common::harness::BIN)
        .parent()
        .expect("a folder")
        .join("examples")
        .join(name);
    assert!(
        path.exists(),
        "build the examples first (cargo build --examples -p aurora): {}",
        path.display()
    );
    path
}

/// Starts runs faster than two slots can finish them, so a queue builds up,
/// cancels some of what waits, and watches the queue's own rules while the
/// server is killed: never more than `MAX_RUNS` runs started at once, and
/// queue positions that are 1, 2, 3 ... in order.
fn runs(shared: &Shared, agent: i64, run: u64) {
    let mut rng = Rng::new(77);
    let mut seq = 0_u64;
    while !shared.stopped() {
        seq += 1;
        let idem = format!("soak-{run}-r-{seq}");
        let Some(made) = shared.send(
            "POST",
            &format!("/api/v1/projects/{PROJECT}/issues"),
            Some(&json!({
                "title": format!("soak-r-{seq}"),
                "type_id": shared.type_id.load(Ordering::SeqCst),
            })),
            Some(&idem),
        ) else {
            return;
        };
        if made.status != 201 {
            shared.violation(format!(
                "create run issue answered {}: {}",
                made.status, made.body
            ));
            return;
        }
        let number = made.json()["number"].as_i64().unwrap_or(0);
        let Some(started) = shared.send(
            "POST",
            &format!("/api/v1/agents/{agent}/runs"),
            Some(&json!({"issue": format!("{PROJECT}-{number}")})),
            Some(&format!("{idem}-run")),
        ) else {
            return;
        };
        match started.status {
            201 => {
                shared.runs_started.fetch_add(1, Ordering::SeqCst);
                let status = started.json()["status"].as_str().unwrap_or("").to_string();
                if status != "queued" && status != "starting" {
                    shared.violation(format!("a new run is `{status}`: {}", started.body));
                    return;
                }
            }
            // The agent's queue is full: the limit doing its job.
            409 => {
                shared.runs_refused.fetch_add(1, Ordering::SeqCst);
            }
            other => {
                shared.violation(format!("starting a run answered {other}: {}", started.body));
                return;
            }
        }
        if rng.below(100) < 30
            && let Some(list) = shared.send("GET", "/api/v1/runs?status=queued", None, None)
            && list.status == 200
        {
            let queued: Vec<i64> = list
                .json()
                .as_array()
                .map(|rows| rows.iter().filter_map(|r| r["id"].as_i64()).collect())
                .unwrap_or_default();
            if !queued.is_empty() {
                let id = queued[rng.below(queued.len() as u64) as usize];
                let Some(canceled) = shared.send(
                    "POST",
                    &format!("/api/v1/runs/{id}/cancel"),
                    None,
                    Some(&format!("{idem}-cancel")),
                ) else {
                    return;
                };
                // 409: it was promoted or finished in the meantime.
                if canceled.status != 200 && canceled.status != 409 {
                    shared.violation(format!(
                        "cancel of run {id} answered {}: {}",
                        canceled.status, canceled.body
                    ));
                    return;
                }
            }
        }
        let Some(queue) = shared.send("GET", "/api/v1/queue", None, None) else {
            return;
        };
        if queue.status == 200 {
            let view = queue.json();
            let running = view["running"].as_i64().unwrap_or(0);
            if running > MAX_RUNS {
                shared.violation(format!(
                    "{running} runs are started at once with a limit of {MAX_RUNS}: {}",
                    queue.body
                ));
                return;
            }
            let positions: Vec<i64> = view["queued"]
                .as_array()
                .map(|rows| rows.iter().filter_map(|r| r["position"].as_i64()).collect())
                .unwrap_or_default();
            let expected: Vec<i64> = (1..=positions.len() as i64).collect();
            if positions != expected {
                shared.violation(format!(
                    "queue positions are {positions:?}, not 1, 2, 3 ..."
                ));
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100 + rng.below(300)));
    }
}

/// A consumer that polls and acknowledges while the server is being killed.
fn consumer(shared: &Shared, run: u64) {
    let name = "soak-consumer";
    let register = shared.send(
        "PUT",
        &format!("/api/v1/consumers/{name}"),
        Some(&json!({"start": "beginning"})),
        Some(&format!("soak-{run}-register")),
    );
    if register.is_none() {
        return;
    }
    let mut n = 0_u64;
    while !shared.stopped() {
        n += 1;
        let Some(polled) = shared.send(
            "GET",
            &format!("/api/v1/consumers/{name}/events?limit=500"),
            None,
            None,
        ) else {
            return;
        };
        if !polled.is_success() {
            shared.violation(format!(
                "consumer poll answered {}: {}",
                polled.status, polled.body
            ));
            return;
        }
        let json = polled.json();
        let cursor = json["consumer"]["last_event_id"].as_i64().unwrap_or(-1);
        let acked = shared.consumer_acked.load(Ordering::SeqCst);
        if cursor < acked {
            shared.violation(format!(
                "the consumer cursor is {cursor}, behind the acknowledged {acked}"
            ));
            return;
        }
        let ids: Vec<i64> = json["events"]
            .as_array()
            .map(|events| events.iter().filter_map(|e| e["id"].as_i64()).collect())
            .unwrap_or_default();
        let expected: Vec<i64> = (cursor + 1..cursor + 1 + ids.len() as i64).collect();
        if ids != expected {
            shared.violation(format!(
                "the consumer at {cursor} was given events {:?}.. instead of consecutive ones",
                ids.iter().take(5).collect::<Vec<_>>()
            ));
            return;
        }
        if let Some(last) = ids.last() {
            let Some(ack) = shared.send(
                "POST",
                &format!("/api/v1/consumers/{name}/ack"),
                Some(&json!({"up_to": last})),
                Some(&format!("soak-{run}-ack-{n}")),
            ) else {
                return;
            };
            if ack.is_success() {
                shared.consumer_acked.fetch_max(*last, Ordering::SeqCst);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Default)]
struct Totals {
    kills: u64,
    restarts: u64,
    library_checks: u64,
    command_checks: u64,
    entries_verified: u64,
}

struct Soak {
    shared: Arc<Shared>,
    db: std::path::PathBuf,
    server: Server,
    totals: Totals,
    rng: Rng,
}

impl Soak {
    fn get(&self, path: &str) -> Option<Value> {
        let addr = self.server.addr.clone();
        match request(&addr, &self.shared.key, "GET", path, None, None) {
            Ok(resp) if resp.status == 200 => Some(resp.json()),
            Ok(resp) => {
                self.shared.violation(format!(
                    "GET {path} answered {}: {}",
                    resp.status, resp.body
                ));
                None
            }
            Err(err) => {
                self.shared.violation(format!("GET {path} failed: {err}"));
                None
            }
        }
    }

    fn verify(&mut self, entry: &Entry) {
        let base = format!("/api/v1/projects/{PROJECT}");
        let (path, pointer, expected): (String, &str, Value) = match entry {
            Entry::Issue { number, title } => {
                (format!("{base}/issues/{number}"), "/title", json!(title))
            }
            Entry::Comment { id, body } => (format!("{base}/comments/{id}"), "/body", json!(body)),
            Entry::Doc { id, title } => (format!("{base}/documents/{id}"), "/title", json!(title)),
            Entry::DocBody { id, body } => (format!("{base}/documents/{id}"), "/body", json!(body)),
            Entry::Column { number, column } => (
                format!("{base}/issues/{number}"),
                "/column_id",
                json!(column),
            ),
            Entry::Archived { number } => {
                (format!("{base}/issues/{number}"), "/archived", json!(true))
            }
            Entry::Link { from, to } => {
                let Some(links) = self.get(&format!("{base}/issues/{from}/links")) else {
                    return;
                };
                let found = links["related"]
                    .as_array()
                    .is_some_and(|list| list.iter().any(|l| l["number"].as_i64() == Some(*to)));
                if !found {
                    self.shared.violation(format!(
                        "an acknowledged link {from} - {to} is missing after the restart"
                    ));
                }
                self.totals.entries_verified += 1;
                return;
            }
        };
        let Some(found) = self.get(&path) else { return };
        self.totals.entries_verified += 1;
        let in_flight = match entry {
            Entry::Column { number, .. } => {
                locked(&self.shared.moving).get(number).is_some_and(|list| {
                    list.iter()
                        .any(|c| Some(c) == found.pointer(pointer).and_then(Value::as_i64).as_ref())
                })
            }
            _ => false,
        };
        if found.pointer(pointer) != Some(&expected) && !in_flight {
            self.shared.violation(format!(
                "{entry:?}: {path} {pointer} is {} after the restart",
                found.pointer(pointer).unwrap_or(&Value::Null)
            ));
        }
    }

    /// Verifies what was acknowledged before the kill, and a sample of older facts.
    fn verify_acknowledged(&mut self) {
        let pending = std::mem::take(&mut locked(&self.shared.ledger).pending);
        let mut latest: BTreeMap<(&'static str, i64), Entry> = BTreeMap::new();
        let mut immutable = Vec::new();
        for entry in pending {
            match entry.slot() {
                Some(slot) => {
                    latest.insert(slot, entry);
                }
                None => immutable.push(entry),
            }
        }
        let sample: Vec<Entry> = {
            let ledger = locked(&self.shared.ledger);
            (0..RESAMPLE.min(ledger.reservoir.len()))
                .map(|_| {
                    let at = self.rng.below(ledger.reservoir.len() as u64) as usize;
                    ledger.reservoir[at].clone()
                })
                .collect()
        };
        for entry in latest
            .into_values()
            .chain(immutable.iter().cloned())
            .chain(sample)
        {
            if self.shared.stopped() && !locked(&self.shared.violations).is_empty() {
                return;
            }
            self.verify(&entry);
        }
        let mut ledger = locked(&self.shared.ledger);
        for entry in immutable.into_iter().filter(Entry::immutable) {
            ledger.seen += 1;
            if ledger.reservoir.len() < RESERVOIR {
                ledger.reservoir.push(entry);
            } else {
                let at = self.rng.below(ledger.seen) as usize;
                if at < RESERVOIR {
                    ledger.reservoir[at] = entry;
                }
            }
        }
    }

    fn check(&mut self, when: &str, command: bool) {
        self.totals.library_checks += 1;
        match aurora_store::invariants::check_file(&self.db) {
            Ok(report) if report.ok() => {}
            Ok(report) => self
                .shared
                .violation(format!("{when}: failed checks {:#?}", report.failed())),
            Err(err) => self.shared.violation(format!(
                "{when}: the check could not open the database: {err}"
            )),
        }
        if command {
            self.totals.command_checks += 1;
            let run = aurora_check(&self.db);
            if run.code != 0 {
                self.shared.violation(format!(
                    "{when}: `aurora check` exited {}:\n{}",
                    run.code, run.stdout
                ));
            }
        }
    }

    fn counts(&self, when: &str) {
        let issues = count(
            &self.db,
            "SELECT COUNT(*) FROM issue WHERE title LIKE 'soak-i-%'",
        );
        let acked = i64::try_from(self.shared.creates_acked.load(Ordering::SeqCst)).unwrap_or(0);
        let in_flight = self.shared.creates_in_flight.load(Ordering::SeqCst) + WORKERS as i64;
        if issues < acked || issues > acked + in_flight {
            self.shared.violation(format!(
                "{when}: {issues} soak issues in the database but {acked} creates acknowledged \
                 (and at most {in_flight} unanswered): a change was lost or applied twice"
            ));
        }
        let started = count(
            &self.db,
            "SELECT COUNT(*) FROM run WHERE status IN ('starting', 'running')",
        );
        if started > MAX_RUNS {
            self.shared.violation(format!(
                "{when}: {started} runs are started at once with a limit of {MAX_RUNS}"
            ));
        }
        let twice = count(
            &self.db,
            "SELECT COUNT(*) FROM (SELECT run_id FROM (SELECT entity_id AS run_id, kind FROM event
              WHERE kind IN ('run.promoted', 'run.finished'))
              GROUP BY run_id, kind HAVING COUNT(*) > 1)",
        );
        if twice != 0 {
            self.shared.violation(format!(
                "{when}: {twice} runs were promoted or finished more than once"
            ));
        }
        let duplicated = count(
            &self.db,
            "SELECT COUNT(*) FROM (SELECT title FROM issue WHERE title LIKE 'soak-i-%'
              GROUP BY title HAVING COUNT(*) > 1)",
        );
        if duplicated != 0 {
            self.shared.violation(format!(
                "{when}: {duplicated} issue titles exist more than once: a retry was applied twice"
            ));
        }
    }

    /// Kill, check the crashed files, restart, check again, verify, resume.
    fn cycle(&mut self, number: u64) {
        self.shared.set_addr(None);
        self.server.kill();
        self.totals.kills += 1;
        let when = format!("kill {number}");
        self.check(
            &format!("{when}, before the restart"),
            number.is_multiple_of(COMMAND_CHECK_EVERY),
        );

        self.server = Server::spawn_with(&self.db, None, SERVER_ENV);
        if let Err(why) = self.server.wait_ready(Duration::from_secs(180)) {
            self.shared
                .violation(format!("{when}: the restart failed: {why}"));
            return;
        }
        self.totals.restarts += 1;
        if !self.server.log_text().contains("unclean_shutdown=true") {
            self.shared
                .violation(format!("{when}: the restart did not notice the kill"));
        }
        self.check(
            &format!("{when}, after the restart"),
            number.is_multiple_of(COMMAND_CHECK_EVERY),
        );
        self.verify_acknowledged();
        self.counts(&when);
        if !self.shared.stopped() {
            self.shared.set_addr(Some(self.server.addr.clone()));
        }
    }
}

fn pause(shared: &Shared, total: Duration) {
    let started = Instant::now();
    while started.elapsed() < total && !shared.stopped() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "runs for AURORA_SOAK_SECONDS (default 120); see the module docs"]
fn kill_loop_soak() {
    let seconds: u64 = std::env::var("AURORA_SOAK_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let run = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!("soak: {seconds} s on {}", db.display());

    let mut server = Server::spawn_with(&db, None, SERVER_ENV);
    server
        .wait_ready(Duration::from_secs(60))
        .expect("the first start");
    let shared = Arc::new(Shared {
        addr: Mutex::new(None),
        key: key.clone(),
        stop: AtomicBool::new(false),
        ledger: Mutex::new(Ledger::default()),
        violations: Mutex::new(Vec::new()),
        acked: AtomicU64::new(0),
        creates_acked: AtomicU64::new(0),
        creates_in_flight: AtomicI64::new(0),
        resends: AtomicU64::new(0),
        replays: AtomicU64::new(0),
        duplicate_probes: AtomicU64::new(0),
        consumer_acked: AtomicI64::new(0),
        runs_started: AtomicU64::new(0),
        runs_refused: AtomicU64::new(0),
        columns: Mutex::new(Vec::new()),
        type_id: AtomicI64::new(0),
        moving: Mutex::new(BTreeMap::new()),
    });

    let setup = |method: &'static str, path: &str, body: Value| -> Value {
        let resp = request(&server.addr, &key, method, path, Some(&body), None).expect("setup");
        assert!(
            resp.is_success(),
            "setup {path}: {} {}",
            resp.status,
            resp.body
        );
        resp.json()
    };
    let project = setup(
        "POST",
        "/api/v1/projects",
        json!({"key": PROJECT, "name": "Soak"}),
    );
    *locked(&shared.columns) = project["columns"]
        .as_array()
        .map(|c| c.iter().filter_map(|c| c["id"].as_i64()).collect())
        .unwrap_or_default();
    let task = setup(
        "POST",
        &format!("/api/v1/projects/{PROJECT}/issue-types"),
        json!({"name": "Task"}),
    );
    shared
        .type_id
        .store(task["id"].as_i64().unwrap_or(0), Ordering::SeqCst);
    let agent = setup(
        "POST",
        "/api/v1/agents",
        json!({"name": "soak-bot", "command": fake_agent().display().to_string(),
               "args": ["--sleep", "2"], "projects": [PROJECT], "max_concurrent": 2,
               "on_assign": false, "on_mention": false}),
    )["id"]
        .as_i64()
        .expect("agent id");
    shared.set_addr(Some(server.addr.clone()));

    let mut handles = Vec::new();
    {
        let shared = shared.clone();
        handles.push(std::thread::spawn(move || runs(&shared, agent, run)));
    }
    for id in 0..WORKERS {
        let shared = shared.clone();
        handles.push(std::thread::spawn(move || worker(&shared, id, run)));
    }
    {
        let shared = shared.clone();
        handles.push(std::thread::spawn(move || consumer(&shared, run)));
    }

    let mut soak = Soak {
        shared: shared.clone(),
        db: db.clone(),
        server,
        totals: Totals::default(),
        rng: Rng::new(99),
    };
    let mut number = 0;
    while Instant::now() < deadline && !shared.stopped() {
        let wait = Duration::from_millis(2000 + soak.rng.below(8001));
        pause(
            &shared,
            wait.min(deadline.saturating_duration_since(Instant::now())),
        );
        if shared.stopped() {
            break;
        }
        number += 1;
        soak.cycle(number);
        if number % 10 == 0 {
            println!(
                "soak: {number} kills, {} writes acknowledged, {:.0} s elapsed",
                shared.acked.load(Ordering::SeqCst),
                started.elapsed().as_secs_f64()
            );
        }
    }

    // Let the clients finish what they are doing while the server is up.
    shared.stop.store(true, Ordering::SeqCst);
    for handle in handles {
        let _ = handle.join();
    }
    let violated_before_final = !locked(&shared.violations).is_empty();
    if !violated_before_final {
        // The final round: the stop flag is only a flag for the clients now.
        shared.stop.store(false, Ordering::SeqCst);
        number += 1;
        soak.cycle(number);
        shared.stop.store(true, Ordering::SeqCst);
        soak.server.kill();
        soak.totals.kills += 1;
        soak.check("the final kill", true);
        // Agents of the killed servers die with them (job objects) or end by
        // themselves after two seconds; let them go before the data folder is
        // removed, then prove none of the agents the soak started is left: every
        // recorded (pid, creation time) must be gone.
        std::thread::sleep(Duration::from_secs(3));
        assert_checks_green_or_note(&soak);
        let strays = stray_agents(&db);
        if !strays.is_empty() {
            locked(&shared.violations).push(format!(
                "fake agents still running after the final shutdown: {strays:?}"
            ));
        }
    }

    let violations = locked(&shared.violations).clone();
    let size = std::fs::metadata(&db).map_or(0, |m| m.len());
    let events = count(&db, "SELECT COUNT(*) FROM event");
    println!("---- soak summary ----");
    println!(
        "duration:              {:.1} s",
        started.elapsed().as_secs_f64()
    );
    println!("kills:                 {}", soak.totals.kills);
    println!("restarts:              {}", soak.totals.restarts);
    println!(
        "writes acknowledged:   {}",
        shared.acked.load(Ordering::SeqCst)
    );
    println!(
        "issue creates acked:   {}",
        shared.creates_acked.load(Ordering::SeqCst)
    );
    println!(
        "requests resent:       {}",
        shared.resends.load(Ordering::SeqCst)
    );
    println!(
        "resends seen replayed: {}",
        shared.replays.load(Ordering::SeqCst)
    );
    println!(
        "duplicate probes:      {}",
        shared.duplicate_probes.load(Ordering::SeqCst)
    );
    println!(
        "consumer acked up to:  {}",
        shared.consumer_acked.load(Ordering::SeqCst)
    );
    println!(
        "checks run:            {} library, {} `aurora check`",
        soak.totals.library_checks, soak.totals.command_checks
    );
    println!("entries re-verified:   {}", soak.totals.entries_verified);
    println!(
        "runs:                  {} started (HTTP 201), {} refused (queue full), {} in the table",
        shared.runs_started.load(Ordering::SeqCst),
        shared.runs_refused.load(Ordering::SeqCst),
        count(&db, "SELECT COUNT(*) FROM run")
    );
    for status in [
        "queued",
        "starting",
        "running",
        "succeeded",
        "failed",
        "interrupted",
        "canceled",
    ] {
        println!(
            "  runs {status:<12} {}",
            count(
                &db,
                &format!("SELECT COUNT(*) FROM run WHERE status = '{status}'")
            )
        );
    }
    println!(
        "  run.promoted events  {}",
        count(
            &db,
            "SELECT COUNT(*) FROM event WHERE kind = 'run.promoted'"
        )
    );
    println!("database:              {size} bytes, {events} events");
    println!("violations:            {}", violations.len());
    for violation in &violations {
        println!("  - {violation}");
    }
    assert!(violations.is_empty(), "the soak saw violations");
    assert!(soak.totals.kills >= 1, "the soak ran no kill cycle");
}

fn assert_checks_green_or_note(soak: &Soak) {
    if locked(&soak.shared.violations).is_empty() {
        assert_checks_green(&soak.db, "after the soak");
    }
}
