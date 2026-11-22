//! The bounded capture of a process's output: only the last bytes are kept in
//! the tail (it goes on the run row), every byte also goes to the transcript
//! when there is one, and the pipes are always drained so a chatty child never
//! blocks.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::transcript::{Stream, TranscriptWriter};

/// The last `cap` bytes written, however much was.
pub struct Tail {
    buf: Vec<u8>,
    cap: usize,
    dropped: bool,
}

impl Tail {
    pub fn new(cap: usize) -> Tail {
        Tail {
            buf: Vec::new(),
            cap,
            dropped: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        // Trimmed in steps so a long stream is not shifted on every read.
        if self.buf.len() > self.cap * 2 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.buf.len() > self.cap {
            let cut = self.buf.len() - self.cap;
            self.buf.drain(..cut);
            self.dropped = true;
        }
    }

    /// The kept text (lossy UTF-8) and whether older output was dropped.
    pub fn finish(&mut self) -> (String, bool) {
        self.trim();
        let mut start = 0;
        if self.dropped {
            // The cut may have split a character: skip its continuation bytes.
            while self.buf.get(start).is_some_and(|b| b & 0xC0 == 0x80) {
                start += 1;
            }
        }
        (
            String::from_utf8_lossy(&self.buf[start..]).into_owned(),
            self.dropped,
        )
    }
}

pub type SharedTail = Arc<Mutex<Tail>>;

/// Where a pipe's output goes besides the tail.
pub struct Sink {
    pub writer: TranscriptWriter,
    pub stream: Stream,
}

/// Reads `reader` to its end into the tail and the transcript. Never stops
/// early on a full tail or a full transcript.
pub async fn pump<R: AsyncRead + Unpin>(mut reader: R, tail: SharedTail, sink: Option<Sink>) {
    let mut chunk = vec![0_u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                tail.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(&chunk[..n]);
                if let Some(sink) = &sink {
                    sink.writer.push(sink.stream, &chunk[..n]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_bytes_and_says_so() {
        let mut tail = Tail::new(10);
        for _ in 0..100 {
            tail.push(b"0123456789");
        }
        tail.push(b"END");
        let (text, dropped) = tail.finish();
        assert!(dropped);
        assert_eq!(text.len(), 10);
        assert!(text.ends_with("END"));
    }

    #[test]
    fn short_output_is_kept_whole() {
        let mut tail = Tail::new(10);
        tail.push(b"abc");
        assert_eq!(tail.finish(), ("abc".to_string(), false));
    }

    #[test]
    fn a_cut_inside_a_character_does_not_start_with_garbage() {
        let mut tail = Tail::new(5);
        tail.push("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}x".as_bytes());
        let (text, dropped) = tail.finish();
        assert!(dropped);
        assert!(!text.contains('\u{fffd}'), "{text:?}");
    }
}
