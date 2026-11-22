//! The transcript of a run (TRK-17): everything the agent's process printed,
//! kept in an append-only file `<data folder>/transcripts/<run id>.log` while
//! the process runs and readable afterwards.
//!
//! This module is the whole storage interface: [`TranscriptWriter`] (the runner
//! appends output) and [`read_page`] / [`read_text`] (the API reads it). The
//! runner and the HTTP handlers know nothing about the file format, so a later
//! task (TRK-46, crash-safe transcript writing) can change how the file is
//! written without touching either.
//!
//! # File format
//!
//! UTF-8 text, one JSON object per line (`\n` ends every complete line):
//!
//! ```text
//! {"t":1250,"s":"o","d":"building...\n"}
//! {"t":1310,"s":"e","d":"warning: unused variable\n"}
//! {"t":9800,"s":"x","d":"run canceled"}
//! ```
//!
//! * `t`: milliseconds since the process was started.
//! * `s`: the stream: `o` standard output, `e` standard error, `x` a marker
//!   written by Aurora itself (not output of the process: `run canceled`,
//!   `run timed out`, `server stopped`, `process exited with code 3`,
//!   `transcript truncated at 8388608 bytes`).
//! * `d`: the text. A frame holds one line of one stream, including its line
//!   ending; a longer line is cut into frames of at most 8 KiB. Concatenating
//!   the `d` of the `o` and `e` frames in file order gives back what the
//!   process printed, in the order it arrived (the two pipes are read
//!   concurrently, so the order between the streams is the order of reading,
//!   not a guarantee about the process's own order).
//!
//! Bytes that are not valid UTF-8 are replaced by U+FFFD (the output tail kept
//! on the run row is lossy in the same way). Control characters are escaped by
//! JSON, so the file never holds a raw control character except the `\n` that
//! ends a line.
//!
//! # Writing
//!
//! Output is split into lines before it is written (a line is held back until
//! its `\n` arrives, at most [`FLUSH_EVERY`] or 64 KiB), so the text of a line
//! is scrubbed in one piece: the per-run API key (replaced by `[key]`) and the
//! project's repository path in either slash spelling (replaced by `the
//! repository`, as `gitwork::scrub` does for errors). This is best effort:
//! only exact occurrences are found, and a secret split across a flush of an
//! unterminated line (written slower than one piece per 250 ms without a line
//! ending) or across a 64 KiB line is not.
//!
//! Frames are appended in batches: a batch is written when 8 KiB are pending or
//! 250 ms passed, so a crash loses at most that. There is no per-write
//! `sync_data` (the file is an append log, not a rename-into-place file);
//! [`TranscriptWriter::finish`] syncs once when the run ends. The file is
//! capped (default 8 MiB, `AURORA_TRANSCRIPT_MAX_BYTES`): the frame that would
//! pass the cap is dropped together with everything after it, one `x` frame
//! `transcript truncated at N bytes` is appended, and the process keeps
//! running (its pipes are still drained). Markers are not counted against the
//! cap. A write error stops the transcript the same way, silently for the run.
//!
//! # Reading
//!
//! [`read_page`] reads from a byte offset, returns only whole lines (up to the
//! last `\n` it saw), so a reader that overlaps the writer never gets a torn
//! line, and a line a crash left half written at the end of the file is simply
//! not returned. A complete line that is not a valid frame is skipped. On
//! Windows the file is opened by the writer with std's default sharing (read,
//! write, delete), so readers can open it while it is appended to.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aurora_store::Store;
use aurora_store::failpoints::{self, Point};
use aurora_store::runs::TranscriptEnd;
use serde::{Deserialize, Serialize};

/// The default size cap of one transcript file.
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The least and the most `AURORA_TRANSCRIPT_MAX_BYTES` may be.
pub const MIN_MAX_BYTES: u64 = 1024;
pub const LARGEST_MAX_BYTES: u64 = 1 << 30;
/// The longest text of one frame, in bytes.
pub const MAX_FRAME_TEXT: usize = 8 * 1024;
/// A batch is written at the latest after this long...
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// ...or as soon as this much is waiting.
const FLUSH_BYTES: usize = 8 * 1024;
/// A line without a line ending is cut here.
const LINE_BUFFER_MAX: usize = 64 * 1024;
/// A page of the reader holds about this much unless asked for less.
pub const DEFAULT_PAGE_BYTES: usize = 64 * 1024;
/// The most one page may hold.
pub const MAX_PAGE_BYTES: usize = 256 * 1024;
/// No line the writer makes is longer; a longer one in a file is not ours.
const MAX_LINE_BYTES: usize = 128 * 1024;

/// The data folder of a store: where its database is.
pub fn data_dir(store: &Store) -> PathBuf {
    let dir = store
        .path()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    std::path::absolute(&dir).unwrap_or(dir)
}

/// Which pipe a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    fn index(self) -> usize {
        match self {
            Stream::Out => 0,
            Stream::Err => 1,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Stream::Out => "o",
            Stream::Err => "e",
        }
    }
}

/// One line of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Milliseconds since the process started.
    pub t: u64,
    /// `o`, `e` or `x`; see the module docs.
    pub s: String,
    pub d: String,
}

impl Entry {
    pub fn is_marker(&self) -> bool {
        self.s == "x"
    }

    fn valid(&self) -> bool {
        matches!(self.s.as_str(), "o" | "e" | "x")
    }
}

/// The line for `text` as it is written, with its `\n`.
pub fn encode(t: u64, stream: &str, text: &str) -> Vec<u8> {
    let entry = Entry {
        t,
        s: stream.to_string(),
        d: text.to_string(),
    };
    // Serializing three plain fields cannot fail; an empty frame would simply
    // be skipped by the reader if it ever did.
    let mut line = serde_json::to_vec(&entry).unwrap_or_default();
    line.push(b'\n');
    line
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- scrubbing ----

/// What must not reach the file.
#[derive(Debug, Clone, Default)]
pub struct Scrubber {
    rules: Vec<(String, &'static str)>,
}

impl Scrubber {
    pub fn new() -> Scrubber {
        Scrubber::default()
    }

    /// Replace `secret` by `[key]` (ignored when empty).
    pub fn key(mut self, secret: &str) -> Scrubber {
        if !secret.is_empty() {
            self.rules.push((secret.to_string(), "[key]"));
        }
        self
    }

    /// Replace the repository path, in both slash spellings, by `the
    /// repository`.
    pub fn repository(mut self, path: &str) -> Scrubber {
        for spelling in [path.to_string(), path.replace('\\', "/")] {
            if !spelling.is_empty() && !self.rules.iter().any(|(n, _)| *n == spelling) {
                self.rules.push((spelling, "the repository"));
            }
        }
        self
    }

    pub fn apply(&self, text: String) -> String {
        let mut out = text;
        for (needle, with) in &self.rules {
            if out.contains(needle.as_str()) {
                out = out.replace(needle.as_str(), with);
            }
        }
        out
    }
}

// ---- writing ----

/// The part of `bytes` that can be turned into text now: all of it unless it
/// ends in the first bytes of a character whose rest has not arrived.
fn complete_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

/// Cuts `text` into pieces of at most [`MAX_FRAME_TEXT`] bytes at character
/// boundaries.
fn pieces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > MAX_FRAME_TEXT {
        let mut cut = MAX_FRAME_TEXT;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        out.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

#[derive(Default)]
struct LineBuf {
    bytes: Vec<u8>,
    /// When the first byte of the unfinished line arrived.
    since: Option<Instant>,
}

struct State {
    path: PathBuf,
    file: Option<File>,
    final_size: u64,
    started: Instant,
    cap: u64,
    /// Bytes of frames accepted so far (written or pending), markers aside.
    accepted: u64,
    truncated: bool,
    closed: bool,
    pending: Vec<u8>,
    lines: [LineBuf; 2],
    scrub: Scrubber,
}

impl State {
    fn t(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn marker(&mut self, text: &str) {
        let frame = encode(self.t(), "x", text);
        self.pending.extend_from_slice(&frame);
    }

    /// Turns one chunk of text of a stream into frames, within the cap.
    fn emit(&mut self, stream: Stream, bytes: &[u8]) {
        if bytes.is_empty() || self.truncated || self.file.is_none() {
            return;
        }
        let text = self
            .scrub
            .apply(String::from_utf8_lossy(bytes).into_owned());
        for piece in pieces(&text) {
            let frame = encode(self.t(), stream.code(), piece);
            if self.accepted + frame.len() as u64 > self.cap {
                self.truncated = true;
                let note = format!("transcript truncated at {} bytes", self.cap);
                self.marker(&note);
                return;
            }
            self.accepted += frame.len() as u64;
            self.pending.extend_from_slice(&frame);
        }
    }

    fn feed(&mut self, stream: Stream, chunk: &[u8]) {
        let index = stream.index();
        if self.lines[index].bytes.is_empty() {
            self.lines[index].since = Some(Instant::now());
        }
        self.lines[index].bytes.extend_from_slice(chunk);
        loop {
            let buf = &self.lines[index].bytes;
            let end = match buf.iter().position(|b| *b == b'\n') {
                Some(at) => at + 1,
                None if buf.len() >= LINE_BUFFER_MAX => complete_prefix(buf),
                None => break,
            };
            if end == 0 {
                break;
            }
            let line: Vec<u8> = self.lines[index].bytes.drain(..end).collect();
            self.emit(stream, &line);
            self.lines[index].since = if self.lines[index].bytes.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
    }

    /// Emits unfinished lines that waited long enough (`all`: whatever there is).
    fn flush_partial(&mut self, all: bool) {
        for stream in [Stream::Out, Stream::Err] {
            let index = stream.index();
            let due = all
                || self.lines[index]
                    .since
                    .is_some_and(|since| since.elapsed() >= FLUSH_EVERY);
            if !due || self.lines[index].bytes.is_empty() {
                continue;
            }
            let end = if all {
                self.lines[index].bytes.len()
            } else {
                complete_prefix(&self.lines[index].bytes)
            };
            if end == 0 {
                continue;
            }
            let part: Vec<u8> = self.lines[index].bytes.drain(..end).collect();
            self.emit(stream, &part);
            self.lines[index].since = if self.lines[index].bytes.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
    }

    /// Appends the pending frames. A write error ends the transcript.
    fn write_pending(&mut self, announce: bool) {
        if self.pending.is_empty() {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            self.pending.clear();
            return;
        };
        let result = file.write_all(&self.pending).and_then(|()| file.flush());
        self.pending.clear();
        match result {
            Ok(()) => {
                if announce {
                    failpoints::hit(Point::RunAfterTranscriptWrite);
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not write the transcript; it stops here");
                self.truncated = true;
                self.file = None;
            }
        }
    }
}

/// Appends a run's output to its transcript file. Cheap to clone; the two
/// pipes of the process share one writer so their output is merged in the
/// order it arrives.
#[derive(Clone)]
pub struct TranscriptWriter {
    state: Arc<Mutex<State>>,
}

impl TranscriptWriter {
    /// Creates (or continues) the file, with its folder.
    pub fn create(path: &Path, cap: u64, scrub: Scrubber) -> std::io::Result<TranscriptWriter> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(TranscriptWriter {
            state: Arc::new(Mutex::new(State {
                path: path.to_path_buf(),
                file: Some(file),
                final_size: 0,
                started: Instant::now(),
                cap,
                accepted: 0,
                truncated: false,
                closed: false,
                pending: Vec::new(),
                lines: [LineBuf::default(), LineBuf::default()],
                scrub,
            })),
        })
    }

    /// Adds a chunk of output (any bytes) from one pipe.
    pub fn push(&self, stream: Stream, chunk: &[u8]) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.feed(stream, chunk);
        if state.pending.len() >= FLUSH_BYTES {
            state.write_pending(true);
        }
    }

    /// Writes what has waited long enough; call about every [`FLUSH_EVERY`].
    pub fn tick(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.flush_partial(false);
        state.write_pending(true);
    }

    /// Writes the rest and the end `marker`, syncs the file and closes it. The
    /// returned size is the file's own, read after the last write, so it is
    /// what a reader and the invariant checker will find. Later pushes are
    /// ignored; calling it again returns the same answer.
    pub fn finish(&self, marker: Option<&str>) -> TranscriptEnd {
        let mut state = lock(&self.state);
        if !state.closed {
            state.flush_partial(true);
            if let Some(text) = marker {
                state.marker(text);
            }
            state.write_pending(false);
            if let Some(file) = state.file.as_ref()
                && let Err(err) = file.sync_data()
            {
                tracing::warn!(error = %err, "could not sync the transcript");
            }
            state.file = None;
            state.closed = true;
            // The file's own size, after the last write and the sync.
            state.final_size =
                std::fs::metadata(&state.path).map_or(state.accepted, |meta| meta.len());
        }
        TranscriptEnd {
            bytes: i64::try_from(state.final_size).unwrap_or(i64::MAX),
            truncated: state.truncated,
        }
    }
}

// ---- reading ----

/// One page of a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Where the next read starts: just after the last whole line returned.
    pub next: u64,
    /// The page reached the end of the file as it was when read: what follows
    /// `next`, if anything, is a line still being written (or torn by a crash).
    pub at_end: bool,
    /// A truncation marker is among the entries.
    pub truncated: bool,
}

fn read_up_to(file: &mut File, at: u64, want: usize) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut buf = Vec::new();
    file.take(want as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Reads whole lines from byte offset `after`, about `limit` bytes of them (a
/// line that does not fit is still returned whole when it is the first).
/// `Ok(None)` when there is no file.
pub fn read_page(path: &Path, after: u64, limit: usize) -> std::io::Result<Option<Page>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let len = file.metadata()?.len();
    let after = after.min(len);
    let limit = limit.clamp(1, MAX_PAGE_BYTES);
    let mut buf = read_up_to(&mut file, after, limit)?;
    let mut end = buf.iter().rposition(|b| *b == b'\n').map(|at| at + 1);
    // The first line is longer than the page: read on and return that line.
    if end.is_none() && after + (buf.len() as u64) < len {
        buf = read_up_to(&mut file, after, MAX_LINE_BYTES)?;
        end = buf.iter().position(|b| *b == b'\n').map(|at| at + 1);
        if end.is_none() && after + (buf.len() as u64) < len {
            // Not a line this writer made: skip it rather than stall.
            return Ok(Some(Page {
                entries: Vec::new(),
                next: after + buf.len() as u64,
                at_end: false,
                truncated: false,
            }));
        }
    }
    let Some(end) = end else {
        // Only the start of a line still being written (or torn by a crash).
        return Ok(Some(Page {
            entries: Vec::new(),
            next: after,
            at_end: true,
            truncated: false,
        }));
    };
    let at_end = after + (buf.len() as u64) >= len && !buf[end..].contains(&b'\n');
    let mut entries = Vec::new();
    for line in buf[..end].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<Entry>(line)
            && entry.valid()
        {
            entries.push(entry);
        }
    }
    let truncated = entries
        .iter()
        .any(|e| e.is_marker() && e.d.starts_with("transcript truncated"));
    Ok(Some(Page {
        entries,
        next: after + end as u64,
        at_end,
        truncated,
    }))
}

/// The whole transcript as text for people: the output of both streams in
/// order, then the markers as lines of the form `[aurora: run canceled]`.
/// Memory is bounded by the transcript's own size cap. `None` without a file.
pub fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    let mut text = String::new();
    let mut at = 0;
    loop {
        let Some(page) = read_page(path, at, MAX_PAGE_BYTES)? else {
            return Ok(None);
        };
        for entry in &page.entries {
            if entry.is_marker() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("[aurora: {}]\n", entry.d));
            } else {
                text.push_str(&entry.d);
            }
        }
        if page.at_end || page.next == at {
            return Ok(Some(text));
        }
        at = page.next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("transcripts").join("1.log")
    }

    fn writer(dir: &tempfile::TempDir, cap: u64, scrub: Scrubber) -> TranscriptWriter {
        TranscriptWriter::create(&file(dir), cap, scrub).unwrap()
    }

    fn all(dir: &tempfile::TempDir) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let page = read_page(&file(dir), at, 64).unwrap().unwrap();
            out.extend(page.entries.clone());
            if page.at_end {
                return out;
            }
            at = page.next;
        }
    }

    #[test]
    fn frames_round_trip_with_control_characters_and_unicode() {
        let text = "tab\there \u{1b}[31mred\u{1b}[0m nul\u{0} \"quoted\" \\ \u{e9}\u{1f600}\r\n";
        let line = encode(42, "o", text);
        assert!(line.ends_with(b"\n") && line.iter().filter(|b| **b == b'\n').count() == 1);
        assert!(
            !line[..line.len() - 1].iter().any(|b| *b < 0x20),
            "no raw control character in the file"
        );
        let entry: Entry = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            entry,
            Entry {
                t: 42,
                s: "o".into(),
                d: text.into()
            }
        );
        assert!(
            String::from_utf8(line)
                .unwrap()
                .starts_with(r#"{"t":42,"s":"o","d":""#)
        );
    }

    #[test]
    fn invalid_utf8_becomes_the_replacement_character_and_split_characters_survive() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        w.push(Stream::Out, b"bad \xff\xfe byte\n");
        // A character split across two reads is joined again.
        let e = "\u{e9}".as_bytes();
        w.push(Stream::Out, &[b'a', e[0]]);
        w.tick();
        w.push(Stream::Out, &[e[1], b'\n']);
        w.finish(None);
        let entries = all(&dir);
        assert_eq!(entries[0].d, "bad \u{fffd}\u{fffd} byte\n");
        let joined: String = entries[1..].iter().map(|e| e.d.as_str()).collect();
        assert_eq!(joined, "a\u{e9}\n");
        assert!(!joined.contains('\u{fffd}'));
    }

    #[test]
    fn lines_are_kept_whole_and_long_ones_are_cut_at_8_kib() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        w.push(Stream::Out, b"one\ntwo\r\nthr");
        w.push(Stream::Err, b"err\n");
        w.push(Stream::Out, b"ee\n");
        let long = "\u{e9}".repeat(10_000);
        w.push(Stream::Out, long.as_bytes());
        w.push(Stream::Out, b"\nlast without ending");
        let end = w.finish(Some("run canceled"));
        let entries = all(&dir);
        let lines: Vec<(&str, &str)> = entries
            .iter()
            .take(4)
            .map(|e| (e.s.as_str(), e.d.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("o", "one\n"),
                ("o", "two\r\n"),
                ("e", "err\n"),
                ("o", "three\n")
            ]
        );
        let pieces: Vec<&Entry> = entries[4..].iter().filter(|e| e.s == "o").collect();
        assert!(pieces.iter().all(|e| e.d.len() <= MAX_FRAME_TEXT));
        let rejoined: String = pieces.iter().map(|e| e.d.as_str()).collect();
        assert_eq!(rejoined, format!("{long}\nlast without ending"));
        let marker = entries.last().unwrap();
        assert_eq!(
            (marker.s.as_str(), marker.d.as_str()),
            ("x", "run canceled")
        );
        assert_eq!(
            end.bytes,
            std::fs::metadata(file(&dir)).unwrap().len() as i64
        );
        assert!(!end.truncated);
    }

    #[test]
    fn the_cap_stops_the_file_and_leaves_one_marker() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, 400, Scrubber::new());
        for n in 0..50 {
            w.push(Stream::Out, format!("line number {n}\n").as_bytes());
        }
        let end = w.finish(Some("run timed out"));
        assert!(end.truncated);
        let entries = all(&dir);
        let data: Vec<&Entry> = entries.iter().filter(|e| !e.is_marker()).collect();
        assert!(!data.is_empty() && data.len() < 50);
        assert_eq!(data[0].d, "line number 0\n", "the start is what is kept");
        let markers: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_marker())
            .map(|e| e.d.as_str())
            .collect();
        assert_eq!(
            markers,
            ["transcript truncated at 400 bytes", "run timed out"]
        );
        let data_bytes: usize = std::fs::metadata(file(&dir)).unwrap().len() as usize;
        assert!(data_bytes <= 400 + 200, "markers are small: {data_bytes}");
        let page = read_page(&file(&dir), 0, 1 << 20).unwrap().unwrap();
        assert!(page.truncated);
    }

    #[test]
    fn the_key_and_the_repository_path_never_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let scrub = Scrubber::new()
            .key("aur_secretsecretsecret")
            .repository(r"C:\work\my repo");
        let w = writer(&dir, DEFAULT_MAX_BYTES, scrub);
        // The key arrives split across two reads of one line.
        w.push(Stream::Out, b"token=aur_secret");
        w.push(Stream::Out, b"secretsecret and C:\\work\\my repo\\src\n");
        w.push(Stream::Err, b"at C:/work/my repo/x.rs\n");
        w.finish(None);
        let raw = std::fs::read_to_string(file(&dir)).unwrap();
        assert!(!raw.contains("aur_secret"), "{raw}");
        assert!(!raw.contains("my repo"), "{raw}");
        let entries = all(&dir);
        assert_eq!(entries[0].d, "token=[key] and the repository\\src\n");
        assert_eq!(entries[1].d, "at the repository/x.rs\n");
    }

    #[test]
    fn a_torn_last_line_is_ignored_and_pages_follow_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        w.push(Stream::Out, b"a\nb\nc\n");
        w.finish(None);
        let path = file(&dir);
        let whole = std::fs::metadata(&path).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(br#"{"t":9,"s":"o","d":"torn"#).unwrap();
        }
        let page = read_page(&path, 0, DEFAULT_PAGE_BYTES).unwrap().unwrap();
        assert_eq!(page.entries.len(), 3);
        assert_eq!(page.next, whole, "next stops before the torn line");
        assert!(page.at_end);
        let again = read_page(&path, page.next, DEFAULT_PAGE_BYTES)
            .unwrap()
            .unwrap();
        assert!(again.entries.is_empty() && again.next == whole && again.at_end);

        // A small limit gives one line at a time, never half of one.
        let first = read_page(&path, 0, 1).unwrap().unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(!first.at_end);
        let second = read_page(&path, first.next, 1).unwrap().unwrap();
        assert_eq!(second.entries[0].d, "b\n");

        // An offset in the middle of a line is the caller's mistake, not a
        // crash: the broken piece is skipped.
        let mid = read_page(&path, 3, DEFAULT_PAGE_BYTES).unwrap().unwrap();
        assert!(mid.next > 3);
        // An offset past the end is clamped.
        let past = read_page(&path, whole + 1000, 10).unwrap().unwrap();
        assert!(past.entries.is_empty() && past.at_end);
        // A complete line that is not a frame is skipped.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\nnot json\n{\"t\":1,\"s\":\"q\",\"d\":\"\"}\n")
                .unwrap();
        }
        let junk = read_page(&path, 0, DEFAULT_PAGE_BYTES).unwrap().unwrap();
        assert_eq!(junk.entries.len(), 3);
        assert!(
            read_page(&dir.path().join("none.log"), 0, 10)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_reader_can_open_the_file_while_the_writer_holds_it_and_never_sees_half_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        let path = file(&dir);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                let mut at = 0;
                let mut seen = 0;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let page = read_page(&path, at, 4096).expect("readable while written");
                    if let Some(page) = page {
                        for entry in &page.entries {
                            assert!(entry.d.starts_with("line "), "{entry:?}");
                            assert!(entry.d.ends_with('\n'));
                            seen += 1;
                        }
                        at = page.next;
                    }
                }
                seen
            })
        };
        for n in 0..2000 {
            w.push(
                Stream::Out,
                format!("line {n} {}\n", "z".repeat(40)).as_bytes(),
            );
            if n % 50 == 0 {
                w.tick();
            }
        }
        w.finish(None);
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let seen = reader.join().unwrap();
        assert!(
            seen > 0,
            "the reader saw lines while the file was being written"
        );
        assert!(seen <= 2000);
    }

    #[test]
    fn the_text_download_joins_the_output_and_shows_markers_as_lines() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        w.push(Stream::Out, b"out\n");
        w.push(Stream::Err, b"err\n");
        w.push(Stream::Out, b"no ending");
        w.finish(Some("run canceled"));
        let text = read_text(&file(&dir)).unwrap().unwrap();
        assert_eq!(text, "out\nerr\nno ending\n[aurora: run canceled]\n");
        assert_eq!(read_text(&dir.path().join("none.log")).unwrap(), None);
    }

    #[test]
    fn finish_is_repeatable_and_later_output_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir, DEFAULT_MAX_BYTES, Scrubber::new());
        w.push(Stream::Out, b"x\n");
        let first = w.finish(Some("server stopped"));
        w.push(Stream::Out, b"late\n");
        w.tick();
        let second = w.finish(Some("again"));
        assert_eq!(first, second);
        assert_eq!(all(&dir).len(), 2);
    }
}
