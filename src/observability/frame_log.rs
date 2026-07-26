//! Opt-in, metadata-only sideband frame forensics.
//!
//! This attributes multibyte corruption without becoming a transcript recorder.
//! Records contain direction, frame kind, byte length, a U+FFFD/UTF-8-fault
//! flag, and optionally the byte offset of the first fault. They never contain
//! text, binary bytes, close reasons, protocol values, excerpts, or hashes.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Primary env var. `OCX_LIVE_FRAME_LOG` is accepted as a compatibility alias
/// so an existing diagnostic workflow keeps working (docs/002 D6).
pub const FRAME_LOG_ENV: &str = "GPT_LIVE_FRAME_LOG";
pub const FRAME_LOG_ENV_ALIAS: &str = "OCX_LIVE_FRAME_LOG";

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
    pub fault_byte_offset: Option<usize>,
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
        let fault_byte_offset = text.find(REPLACEMENT);
        FrameRecord {
            ts: timestamp(),
            dir: dir.as_str(),
            kind: FrameKind::Text.as_str(),
            bytes: text.len(),
            fffd: fault_byte_offset.is_some(),
            fault_byte_offset,
        }
    }

    /// Build the record for a binary frame without writing it.
    ///
    /// The payload is inspected only to find the first literal replacement
    /// character or invalid UTF-8 byte. It is never retained or decoded into a
    /// log field, and the reported length is always the raw byte count.
    pub fn binary_record(&self, dir: Direction, bytes: &[u8]) -> FrameRecord {
        let fault_byte_offset = first_fault_byte(bytes);
        FrameRecord {
            ts: timestamp(),
            dir: dir.as_str(),
            kind: FrameKind::Binary.as_str(),
            bytes: bytes.len(),
            fffd: fault_byte_offset.is_some(),
            fault_byte_offset,
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

/// Return the byte position of the first literal U+FFFD or malformed UTF-8
/// sequence, whichever occurs first, without allocating a payload copy.
fn first_fault_byte(bytes: &[u8]) -> Option<usize> {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.find(REPLACEMENT),
        Err(error) => {
            let invalid_at = error.valid_up_to();
            let valid_prefix = std::str::from_utf8(&bytes[..invalid_at]).unwrap_or("");
            valid_prefix.find(REPLACEMENT).or(Some(invalid_at))
        }
    }
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
            record.fault_byte_offset.is_none(),
            "a clean frame must carry no payload"
        );
        // Byte length, not character count.
        assert_eq!(record.bytes, "가볍게 얘기해봐요".len());
    }

    #[test]
    fn a_replacement_character_yields_only_its_byte_offset() {
        let logger = FrameLogger::disabled();
        let payload = format!("{}\u{FFFD}adjacent-secret", "한".repeat(10));
        let record = logger.text_record(Direction::UpstreamToClient, &payload);

        assert!(record.fffd);
        assert_eq!(record.fault_byte_offset, Some("한".len() * 10));
    }

    #[test]
    fn a_multibyte_prefix_reports_a_byte_not_scalar_offset() {
        let logger = FrameLogger::disabled();
        let payload = "한글\u{FFFD}tail";
        let record = logger.text_record(Direction::ClientToUpstream, payload);
        assert_eq!(record.fault_byte_offset, Some("한글".len()));
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
        assert_eq!(record.fault_byte_offset, Some(1));
    }

    #[test]
    fn invalid_binary_after_multibyte_text_reports_the_exact_byte() {
        let logger = FrameLogger::disabled();
        let mut payload = "한".repeat(400).into_bytes();
        let expected = payload.len();
        payload.push(0xff);

        let record = logger.binary_record(Direction::UpstreamToClient, &payload);
        assert!(record.fffd);
        assert_eq!(record.fault_byte_offset, Some(expected));
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
        assert_eq!(record.fault_byte_offset, Some("head".len()));
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
        assert!(record.fault_byte_offset.is_none());
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
            value
                .as_object()
                .unwrap()
                .get("fault_byte_offset")
                .is_none(),
            "fault_byte_offset must be absent, not null"
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
