//! Opt-in, metadata-only sideband frame forensics.
//!
//! This exists to attribute multibyte corruption — was the replacement
//! character already in the upstream frame, or did the relay introduce it? —
//! without becoming a transcript recorder. The record carries direction, kind,
//! byte length, and a U+FFFD flag. A payload excerpt appears only when a
//! replacement character is present, and only around it.
//!
//! # What this does NOT guarantee
//!
//! The excerpt is *bounded*, not *empty*. For a corrupted frame it contains up
//! to [`CONTEXT_CHARS`] scalars on each side of the replacement character, and
//! for a frame shorter than that window the excerpt is the whole frame. If a
//! secret sits adjacent to the corruption, that secret is in the excerpt.
//!
//! This is a deliberate trade, not an oversight: an excerpt that omitted the
//! surrounding bytes could not attribute corruption at all, which is the only
//! reason the log exists. The mitigations are that it is opt-in, that a clean
//! frame produces no excerpt whatsoever, and that the operator is told to treat
//! the file as sensitive and write it outside the working tree.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Primary env var. `OCX_LIVE_FRAME_LOG` is accepted as a compatibility alias
/// so an existing diagnostic workflow keeps working (docs/002 D6).
pub const FRAME_LOG_ENV: &str = "GPT_LIVE_FRAME_LOG";
pub const FRAME_LOG_ENV_ALIAS: &str = "OCX_LIVE_FRAME_LOG";

/// Unicode scalar values retained on each side of the first U+FFFD.
pub const CONTEXT_CHARS: usize = 24;

/// The replacement character whose presence is the whole point of this log.
const REPLACEMENT: char = '\u{FFFD}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client to upstream.
    ClientToUpstream,
    /// Upstream to client.
    UpstreamToClient,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientToUpstream => "c2u",
            Self::UpstreamToClient => "u2c",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Text,
    Binary,
}

impl FrameKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FrameRecord {
    pub ts: String,
    pub dir: &'static str,
    pub kind: &'static str,
    pub bytes: usize,
    pub fffd: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Inert unless a path is configured.
/// How many records may await the writer before new ones are dropped.
///
/// Forensics is diagnostic: losing a record under pressure is strictly better
/// than stalling a voice relay.
pub const WRITER_QUEUE_DEPTH: usize = 1024;

/// How long shutdown waits for the writer before giving up on the tail.
pub const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct FrameLogger {
    path: Option<PathBuf>,
    /// When present, records are handed to a dedicated writer task instead of
    /// being written inline. A synchronous `open`/`write` inside the relay could
    /// block on a slow disk, a stalled network filesystem, or a FIFO — and
    /// blocking there stops frame forwarding and can occupy a Tokio worker.
    sender: Option<std::sync::mpsc::SyncSender<FrameRecord>>,
    /// Joined by [`FrameLogger::drain`] so shutdown flushes the queue.
    writer: Option<Arc<Mutex<Option<std::thread::JoinHandle<()>>>>>,
}

impl FrameLogger {
    pub fn disabled() -> Self {
        Self {
            path: None,
            sender: None,
            writer: None,
        }
    }

    /// Synchronous writer, used by tests that need to read the file immediately.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            sender: None,
            writer: None,
        }
    }

    /// Spawn a dedicated writer thread and return a logger that never blocks
    /// the caller. Records are dropped when the queue is full.
    ///
    /// The returned logger owns a join handle; call [`FrameLogger::drain`] on
    /// shutdown so queued records are written rather than abandoned.
    pub fn spawn(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (sender, receiver) = std::sync::mpsc::sync_channel::<FrameRecord>(WRITER_QUEUE_DEPTH);
        let writer_path = path.clone();
        let handle = std::thread::Builder::new()
            .name("gpt-live-frame-log".to_string())
            .spawn(move || {
                // Runs until every sender is dropped.
                for record in receiver {
                    let _ = append_line(&writer_path, &record);
                }
            })
            .ok();
        Self {
            path: Some(path),
            sender: Some(sender),
            writer: handle.map(|handle| Arc::new(Mutex::new(Some(handle)))),
        }
    }

    /// Resolve from the environment, spawning a writer when configured.
    ///
    /// Owned by `AppState` rather than a static: a `OnceLock` is never dropped
    /// at process exit, so its writer could never observe "all senders gone"
    /// and queued tail records would be lost — exactly when they matter.
    pub fn from_env_owned() -> Self {
        match Self::from_source(|k| std::env::var(k).ok()).path {
            Some(path) => Self::spawn(path),
            None => Self::disabled(),
        }
    }

    /// Drop this logger's sender and wait, up to `timeout`, for the writer to
    /// finish.
    ///
    /// Bounded on purpose. An upgraded relay holds its own clone and runs in a
    /// detached task, so the channel can still be alive when the server stops
    /// accepting; an unbounded join would then wait forever, and a blocked
    /// filesystem write could make it permanent. Losing the tail of a
    /// diagnostic log is an acceptable cost for a shutdown that terminates.
    pub fn drain_with_timeout(&mut self, timeout: std::time::Duration) -> bool {
        self.sender = None;
        let Some(writer) = self.writer.take() else {
            return true;
        };
        let Ok(mut slot) = writer.lock() else {
            return false;
        };
        let Some(handle) = slot.take() else {
            return true;
        };

        let deadline = std::time::Instant::now() + timeout;
        while !handle.is_finished() {
            if std::time::Instant::now() >= deadline {
                // Leave the thread detached; the process is exiting anyway.
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        handle.join().is_ok()
    }

    /// Drain with the default shutdown budget.
    pub fn drain(&mut self) -> bool {
        self.drain_with_timeout(DRAIN_TIMEOUT)
    }

    /// Environment-agnostic resolver, so tests never mutate process state.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Self {
        let path = get(FRAME_LOG_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                get(FRAME_LOG_ENV_ALIAS)
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            });
        Self {
            path: path.map(PathBuf::from),
            sender: None,
            writer: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Build the record for a text frame without writing it.
    pub fn text_record(&self, dir: Direction, text: &str) -> FrameRecord {
        let context = fffd_context(text);
        FrameRecord {
            ts: timestamp(),
            dir: dir.as_str(),
            kind: FrameKind::Text.as_str(),
            bytes: text.len(),
            fffd: context.is_some(),
            context,
        }
    }

    /// Build the record for a binary frame without writing it.
    ///
    /// The payload is decoded ONLY to detect a replacement character; the
    /// decoded form never feeds back into the relayed frame, and the reported
    /// length is always the raw byte count.
    pub fn binary_record(&self, dir: Direction, bytes: &[u8]) -> FrameRecord {
        let context = binary_context(bytes);
        FrameRecord {
            ts: timestamp(),
            dir: dir.as_str(),
            kind: FrameKind::Binary.as_str(),
            bytes: bytes.len(),
            fffd: context.is_some(),
            context,
        }
    }

    /// Append a text-frame record. Errors are swallowed: forensics must never
    /// break a call.
    pub fn log_text(&self, dir: Direction, text: &str) {
        if self.path.is_none() {
            return;
        }
        self.append(self.text_record(dir, text));
    }

    /// Append a binary-frame record.
    pub fn log_binary(&self, dir: Direction, bytes: &[u8]) {
        if self.path.is_none() {
            return;
        }
        self.append(self.binary_record(dir, bytes));
    }

    fn append(&self, record: FrameRecord) {
        if let Some(sender) = self.sender.as_ref() {
            // `try_send`, never `send`: a full queue drops the record rather
            // than parking the relay task.
            let _ = sender.try_send(record);
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let _ = append_line(path, &record);
    }
}

fn append_line(path: &Path, record: &FrameRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(record)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

/// RFC 3339 without pulling in a date library.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();

    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days algorithm, shifted to a 1970 epoch.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Detect corruption in a binary frame without decoding the whole thing.
///
/// A lossy decode of the entire payload would allocate up to three times its
/// size just to produce a 49-scalar excerpt, so this finds the first invalid
/// sequence and decodes only a bounded window around it. A frame that is valid
/// UTF-8 is inspected for a literal replacement character in the same bounded
/// way.
fn binary_context(bytes: &[u8]) -> Option<String> {
    // A generous byte window: enough to yield CONTEXT_CHARS scalars on each
    // side even for 4-byte characters.
    const WINDOW: usize = CONTEXT_CHARS * 4;

    let error_at = match std::str::from_utf8(bytes) {
        Ok(text) => return fffd_context(text),
        Err(err) => err.valid_up_to(),
    };

    // The prefix up to `error_at` is known-valid UTF-8. If it ALREADY contains
    // a literal replacement character, that one is the first occurrence in the
    // conceptual lossy decode, so centre on it rather than on the later invalid
    // sequence — otherwise the "first occurrence" rule would silently depend on
    // whether the corruption was literal or malformed.
    let valid_prefix = std::str::from_utf8(&bytes[..error_at]).unwrap_or("");
    if let Some(context) = fffd_context(valid_prefix) {
        return Some(context);
    }

    // Window start chosen from the prefix's char boundaries: slicing at a raw
    // byte offset could split a character and manufacture a replacement
    // character that the excerpt would then centre on.
    let mut start = error_at;
    for (index, _) in valid_prefix.char_indices().rev() {
        if error_at - index > WINDOW {
            break;
        }
        start = index;
    }

    let end = bytes.len().min(error_at.saturating_add(WINDOW));
    let decoded = String::from_utf8_lossy(&bytes[start..end]);
    fffd_context(&decoded)
}

/// A bounded excerpt around the first replacement character, or `None`.
///
/// Bounded in Unicode scalar values with char-boundary clamping. The
/// TypeScript original slices UTF-16 code units with an exclusive end (up to 24
/// before, at most 23 after); the invariant preserved here is a *bounded*
/// excerpt, not the exact unit count (docs/001 §8).
///
/// "Bounded" is the whole claim. A frame shorter than the window yields the
/// whole frame — see the module docs.
fn fffd_context(text: &str) -> Option<String> {
    let index = text.find(REPLACEMENT)?;
    let before: String = text[..index]
        .chars()
        .rev()
        .take(CONTEXT_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = text[index..].chars().take(CONTEXT_CHARS + 1).collect();
    Some(format!("{before}{after}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn the_logger_is_inert_without_a_configured_path() {
        let logger = FrameLogger::from_source(source(&[]));
        assert!(!logger.is_enabled());
        // Must not panic or write anywhere.
        logger.log_text(Direction::ClientToUpstream, "anything");
        logger.log_binary(Direction::UpstreamToClient, &[0xff]);
    }

    #[test]
    fn an_empty_value_does_not_enable_logging() {
        let logger = FrameLogger::from_source(source(&[(FRAME_LOG_ENV, "   ")]));
        assert!(!logger.is_enabled());
    }

    #[test]
    fn the_legacy_alias_is_accepted_and_the_primary_wins() {
        let alias = FrameLogger::from_source(source(&[(FRAME_LOG_ENV_ALIAS, "/tmp/a.jsonl")]));
        assert!(alias.is_enabled());

        let both = FrameLogger::from_source(source(&[
            (FRAME_LOG_ENV, "/tmp/primary.jsonl"),
            (FRAME_LOG_ENV_ALIAS, "/tmp/alias.jsonl"),
        ]));
        assert_eq!(both.path.unwrap().to_str().unwrap(), "/tmp/primary.jsonl");
    }

    #[test]
    fn a_clean_text_frame_records_no_excerpt() {
        let logger = FrameLogger::disabled();
        let record = logger.text_record(Direction::ClientToUpstream, "가볍게 얘기해봐요");
        assert_eq!(record.dir, "c2u");
        assert_eq!(record.kind, "text");
        assert!(!record.fffd);
        assert!(
            record.context.is_none(),
            "a clean frame must carry no payload"
        );
        // Byte length, not character count.
        assert_eq!(record.bytes, "가볍게 얘기해봐요".len());
    }

    #[test]
    fn a_replacement_character_yields_a_bounded_excerpt() {
        let logger = FrameLogger::disabled();
        let payload = format!("{}\u{FFFD}{}", "a".repeat(500), "b".repeat(500));
        let record = logger.text_record(Direction::UpstreamToClient, &payload);

        assert!(record.fffd);
        let context = record.context.expect("an excerpt");
        // Never the full payload.
        assert!(
            context.len() < payload.len() / 10,
            "excerpt too long: {context}"
        );
        assert!(context.contains(REPLACEMENT));
        assert_eq!(context.chars().filter(|c| *c == 'a').count(), CONTEXT_CHARS);
        assert_eq!(context.chars().filter(|c| *c == 'b').count(), CONTEXT_CHARS);
    }

    #[test]
    fn the_excerpt_clamps_to_char_boundaries() {
        let logger = FrameLogger::disabled();
        // Multibyte on both sides: a byte-indexed slice would panic here.
        let payload = format!("{}\u{FFFD}{}", "한".repeat(100), "글".repeat(100));
        let record = logger.text_record(Direction::ClientToUpstream, &payload);
        let context = record.context.expect("an excerpt");
        assert!(context.chars().count() <= CONTEXT_CHARS * 2 + 1);
    }

    #[test]
    fn a_short_payload_is_not_padded() {
        let logger = FrameLogger::disabled();
        let record = logger.text_record(Direction::ClientToUpstream, "a\u{FFFD}b");
        assert_eq!(record.context.unwrap(), "a\u{FFFD}b");
    }

    #[test]
    fn a_binary_frame_reports_raw_bytes_and_detects_corruption() {
        let logger = FrameLogger::disabled();
        // Invalid UTF-8: lossy decoding produces a replacement character.
        let record = logger.binary_record(Direction::UpstreamToClient, &[0x61, 0xff, 0x62]);
        assert_eq!(record.kind, "binary");
        assert_eq!(
            record.bytes, 3,
            "the raw byte count, not the decoded length"
        );
        assert!(record.fffd);
    }

    /// The window start must land on a character boundary of the known-valid
    /// prefix. Slicing at a raw byte offset could split a multi-byte character
    /// and manufacture a replacement character at the slice start, and the
    /// excerpt would then centre on that artefact instead of the real
    /// corruption.
    #[test]
    fn the_binary_window_does_not_manufacture_corruption() {
        let logger = FrameLogger::disabled();
        // Multibyte filler well past the window, then the real corruption.
        let mut payload = "한".repeat(400).into_bytes();
        payload.push(0xff);
        payload.extend_from_slice("tail".as_bytes());

        let record = logger.binary_record(Direction::UpstreamToClient, &payload);
        assert!(record.fffd);
        let context = record.context.expect("an excerpt");

        // The excerpt must show the corruption in its true surroundings.
        assert!(
            context.contains("tail"),
            "the excerpt missed the real corruption site: {context}"
        );
        assert!(context.chars().count() <= CONTEXT_CHARS * 2 + 1);
        assert_eq!(record.bytes, payload.len());
    }

    /// A literal U+FFFD earlier in the frame is the first occurrence, even when
    /// a malformed sequence appears later.
    #[test]
    fn an_earlier_literal_replacement_wins_over_a_later_invalid_byte() {
        let logger = FrameLogger::disabled();
        let mut payload = format!("head\u{FFFD}marker{}", "x".repeat(500)).into_bytes();
        payload.push(0xff);

        let record = logger.binary_record(Direction::UpstreamToClient, &payload);
        assert!(record.fffd);
        let context = record.context.expect("an excerpt");
        assert!(
            context.contains("head") && context.contains("marker"),
            "the excerpt centred on the wrong occurrence: {context}"
        );
    }

    #[test]
    fn a_large_valid_binary_frame_is_inspected_without_a_lossy_copy() {
        let logger = FrameLogger::disabled();
        let payload = "ok".repeat(100_000).into_bytes();
        let record = logger.binary_record(Direction::ClientToUpstream, &payload);
        assert!(!record.fffd);
        assert_eq!(record.bytes, payload.len());
    }

    #[test]
    fn a_valid_binary_frame_is_not_flagged() {
        let logger = FrameLogger::disabled();
        let record = logger.binary_record(Direction::ClientToUpstream, "ok".as_bytes());
        assert!(!record.fffd);
        assert!(record.context.is_none());
    }

    #[test]
    fn records_serialize_as_the_documented_jsonl_shape() {
        let logger = FrameLogger::disabled();
        let record = logger.text_record(Direction::ClientToUpstream, "clean");
        let value: serde_json::Value = serde_json::to_value(&record).unwrap();
        assert!(value["ts"].is_string());
        assert_eq!(value["dir"], "c2u");
        assert_eq!(value["kind"], "text");
        assert_eq!(value["bytes"], 5);
        assert_eq!(value["fffd"], false);
        assert!(
            value.as_object().unwrap().get("context").is_none(),
            "context must be absent, not null"
        );
    }

    #[test]
    fn an_unwritable_path_does_not_panic() {
        let logger = FrameLogger::new("/nonexistent-directory-cf83e1/frames.jsonl");
        assert!(logger.is_enabled());
        // Forensics must never break a relay.
        logger.log_text(Direction::ClientToUpstream, "x");
        logger.log_binary(Direction::UpstreamToClient, &[0x00]);
    }

    #[test]
    fn writing_appends_one_line_per_frame() {
        let dir = std::env::temp_dir().join(format!("gpt-live-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frames.jsonl");
        let _ = std::fs::remove_file(&path);

        let logger = FrameLogger::new(&path);
        logger.log_text(Direction::ClientToUpstream, "one");
        logger.log_binary(Direction::UpstreamToClient, &[0x61]);

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value["ts"].is_string());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_timestamp_is_rfc3339_shaped() {
        let ts = timestamp();
        assert_eq!(ts.len(), 24, "YYYY-MM-DDTHH:MM:SS.mmmZ");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn the_civil_calendar_conversion_is_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, to catch an off-by-one in the era arithmetic.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
