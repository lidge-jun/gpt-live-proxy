//! End-to-end proof that frame forensics stays metadata-only on a live relay.

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use gpt_live_proxy::app::{router, AppState};
use gpt_live_proxy::config::{BearerToken, Config, UpstreamProfile};
use gpt_live_proxy::observability::{Direction, FrameLogger};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

fn frameless_request(url: String) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = url.into_client_request().expect("frameless request");
    request.headers_mut().insert(
        "openai-alpha",
        "quicksilver=v2".parse().expect("static alpha"),
    );
    request
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gpt-live-forensics-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect()
}

#[test]
fn a_clean_transcript_never_reaches_the_log() {
    let path = temp_path("clean.jsonl");
    let logger = FrameLogger::new(&path);

    // Realistic transcript content, entirely valid.
    let transcript = "사용자가 방금 말한 민감한 내용입니다. Account 4111-1111-1111-1111.";
    logger.log_text(Direction::UpstreamToClient, transcript);

    let records = read_records(&path);
    assert_eq!(records.len(), 1);
    let record = &records[0];

    assert_eq!(record["fffd"], false);
    assert!(
        record
            .as_object()
            .unwrap()
            .get("fault_byte_offset")
            .is_none(),
        "a clean frame must carry no fault offset"
    );

    // The decisive assertion: none of the payload is anywhere in the file.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("민감한"), "transcript text leaked: {raw}");
    assert!(!raw.contains("4111"), "transcript text leaked: {raw}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_corrupted_text_frame_logs_only_metadata_even_next_to_a_credential() {
    let path = temp_path("corrupt.jsonl");
    let logger = FrameLogger::new(&path);

    let prefix = "머리말".repeat(20);
    let canary = "Bearer adjacent-text-credential-canary";
    let payload = format!("{prefix}\u{FFFD}{canary}");
    logger.log_text(Direction::UpstreamToClient, &payload);

    let records = read_records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["fffd"], true);
    assert_eq!(
        records[0]["fault_byte_offset"],
        serde_json::json!(prefix.len())
    );

    let raw = std::fs::read_to_string(&path).unwrap();
    for forbidden in [
        "머리말",
        "\u{FFFD}",
        canary,
        "adjacent-text-credential-canary",
    ] {
        assert!(!raw.contains(forbidden), "frame payload leaked: {raw}");
    }
    assert!(records[0].as_object().unwrap().get("context").is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn adjacent_text_payload_is_never_serialized() {
    let path = temp_path("credentials.jsonl");
    let logger = FrameLogger::new(&path);

    logger.log_text(
        Direction::ClientToUpstream,
        "openai-insecure-api-key.adjacent-browser-canary\u{FFFD}",
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains("adjacent-browser-canary"),
        "adjacent credential leaked: {raw}"
    );
    assert!(!raw.contains("openai-insecure-api-key"));
    assert!(!raw.contains("context"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_append_failure_is_not_fatal() {
    // A directory that cannot be written to.
    let logger = FrameLogger::new("/nonexistent-cf83e1357eefb8bd/frames.jsonl");
    assert!(logger.is_enabled());

    // Many frames, all failing to write; the caller must survive every one.
    for index in 0..100 {
        logger.log_text(Direction::ClientToUpstream, &format!("frame {index}"));
        logger.log_binary(Direction::UpstreamToClient, &[0xff, 0xfe]);
    }
}

#[test]
fn binary_frames_report_raw_length_and_never_serialize_adjacent_bytes() {
    let path = temp_path("binary.jsonl");
    let logger = FrameLogger::new(&path);

    let mut payload = b"binary-credential-canary".to_vec();
    let fault_at = payload.len();
    payload.push(0xff);
    payload.extend_from_slice(b"adjacent-binary-secret");
    logger.log_binary(Direction::UpstreamToClient, &payload);

    let records = read_records(&path);
    assert_eq!(records[0]["kind"], "binary");
    assert_eq!(records[0]["bytes"], payload.len());
    assert_eq!(records[0]["fault_byte_offset"], fault_at);

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("binary-credential-canary"));
    assert!(!raw.contains("adjacent-binary-secret"));
    assert!(!raw.contains("context"));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Through a live relay
// ---------------------------------------------------------------------------

async fn start_echo_upstream() -> String {
    async fn upgrade(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Close(frame) => {
                        let _ = socket.send(Message::Close(frame)).await;
                        return;
                    }
                    other => {
                        if socket.send(other).await.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    }

    let app = Router::new().fallback(get(upgrade));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Inject the logger rather than setting a process env var: two tests that
/// each need a different log path cannot share process state.
async fn start_proxy(upstream: &str, frame_log: FrameLogger) -> String {
    let mut config = Config::from_source(|k| match k {
        "GPT_LIVE_TOKEN" => Some("unused".to_string()),
        _ => None,
    })
    .expect("config");
    config.upstream = UpstreamProfile::ApiKeyManaged {
        base_url: format!("{upstream}/v1"),
        auth: BearerToken::new("sk-test"),
    };

    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    config.bind = addr;
    let mut state = AppState::new(config).expect("state");
    state.frame_log = frame_log;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn a_live_relay_logs_both_directions_and_excludes_keepalives() {
    let path = temp_path("relay.jsonl");
    // A synchronous logger: the test can read the file without racing a writer.
    let proxy = start_proxy(&start_echo_upstream().await, FrameLogger::new(&path)).await;

    let (mut client, _) =
        tokio_tungstenite::connect_async(frameless_request(format!("{proxy}/v1/live/rtc_log")))
            .await
            .expect("upgrade");

    // A ping must not appear in the log at all.
    client
        .send(ClientMessage::Ping(Vec::new().into()))
        .await
        .unwrap();
    client
        .send(ClientMessage::Text("hello".into()))
        .await
        .unwrap();
    let _ = client.next().await;

    for _ in 0..50 {
        if read_records(&path).len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let records = read_records(&path);
    let directions: Vec<&str> = records.iter().filter_map(|r| r["dir"].as_str()).collect();

    assert!(
        directions.contains(&"c2u"),
        "the client-to-upstream frame was not logged: {records:?}"
    );
    assert!(
        directions.contains(&"u2c"),
        "the upstream-to-client frame was not logged: {records:?}"
    );
    assert_eq!(
        records.len(),
        2,
        "exactly the two data frames; a keepalive was logged: {records:?}"
    );

    let _ = std::fs::remove_file(&path);
}

/// A failing log must not stop the relay from carrying frames.
#[tokio::test]
async fn a_live_relay_survives_an_unwritable_log() {
    let proxy = start_proxy(
        &start_echo_upstream().await,
        FrameLogger::new("/nonexistent-cf83e1357eefb8bd/f.jsonl"),
    )
    .await;

    let (mut client, _) =
        tokio_tungstenite::connect_async(frameless_request(format!("{proxy}/v1/live/rtc_fail")))
            .await
            .expect("upgrade");

    for index in 0..5 {
        client
            .send(ClientMessage::Text(format!("frame-{index}").into()))
            .await
            .expect("send");
        match client.next().await.expect("reply").expect("frame") {
            ClientMessage::Text(text) => assert_eq!(text.as_str(), format!("frame-{index}")),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

/// The spawned writer must flush what it holds when the service drains it.
#[tokio::test]
async fn draining_flushes_queued_records() {
    let path = temp_path("drain.jsonl");
    let mut logger = FrameLogger::spawn(&path);

    for index in 0..64 {
        logger.log_text(Direction::ClientToUpstream, &format!("queued-{index}"));
    }

    // Without the join, the process could exit with records still in flight.
    assert!(
        logger.drain(),
        "the writer should finish well inside the budget"
    );

    assert_eq!(
        read_records(&path).len(),
        64,
        "drain must flush every queued record"
    );

    let _ = std::fs::remove_file(&path);
}

/// A clone held elsewhere keeps the channel alive, so drain must give up rather
/// than wait forever — which is what an upgraded relay in a detached task does.
#[tokio::test]
async fn draining_is_bounded_when_a_clone_outlives_the_service() {
    let path = temp_path("bounded-drain.jsonl");
    let mut logger = FrameLogger::spawn(&path);
    let held = logger.clone();

    let started = std::time::Instant::now();
    let flushed = logger.drain_with_timeout(std::time::Duration::from_millis(200));
    let elapsed = started.elapsed();

    assert!(!flushed, "a live clone should prevent a clean finish");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "drain must be bounded, took {elapsed:?}"
    );

    drop(held);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

/// Capture tracing output so a regression in the span/event contract fails a
/// test rather than being noticed in production.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }

    /// The span-CLOSE line only.
    ///
    /// Scanning the whole output cannot prove a SPAN carries a field: the
    /// terminal event emits `status` and `elapsed_ms` too, so removing the
    /// `span.record` calls would still satisfy a whole-output search. The CLOSE
    /// line renders exactly the span's own fields, so isolating it is what makes
    /// the assertion meaningful.
    fn close_line(&self, span_name: &str) -> String {
        self.text()
            .lines()
            .find(|line| {
                line.contains(&format!("{span_name}{{")) && line.contains("close time.busy")
            })
            .unwrap_or_default()
            .to_string()
    }
}

#[tokio::test]
async fn call_create_logs_carry_the_contract_fields_and_no_secrets() {
    let logs = CapturedLogs::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        // CI colourizes by default, and the escape codes land between the span
        // name and its brace, which breaks the CLOSE-line matcher below. Locally
        // this is a no-op because there is no TTY.
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        // Render span fields, so the assertions below test the SPAN contract
        // rather than the terminal event's own fields.
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();

    let upstream = start_echo_upstream().await;
    let proxy_http;
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        proxy_http = start_proxy(&upstream, FrameLogger::disabled())
            .await
            .replace("ws://", "http://");

        // Rejected during the body read, before any upstream contact, so the
        // legacy Live span must already carry the host. The official
        // `/v1/realtime/calls` route now validates its media type before body
        // read, so this observability contract belongs on `/v1/live`.
        let _ = reqwest::Client::new()
            .post(format!("{proxy_http}/v1/live"))
            .header("openai-alpha", "quicksilver=v2")
            .header("content-type", "multipart/form-data; boundary=forensics")
            .header("authorization", "Bearer client-token-should-not-appear")
            .body(vec![b'x'; 32 * 1024 * 1024])
            .send()
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let text = logs.text();
    assert!(text.contains("call_create"), "span name missing: {text}");

    // Isolated to the span's own rendering, so an event carrying the same value
    // cannot satisfy these.
    let span_line = logs.close_line("call_create");
    assert!(
        !span_line.is_empty(),
        "no call_create span CLOSE line was emitted: {text}"
    );
    for expected in [
        "method=POST",
        "path=/v1/live",
        "upstream=\"127.0.0.1:",
        "status=413",
        "elapsed_ms=",
    ] {
        assert!(
            span_line.contains(expected),
            "the span itself is missing {expected}: {span_line}"
        );
    }

    assert!(
        !text.contains("client-token-should-not-appear"),
        "a client credential reached the logs: {text}"
    );
    assert!(
        !text.contains("sk-test"),
        "the upstream credential reached the logs: {text}"
    );
}

#[tokio::test]
async fn sideband_logs_carry_the_span_contract() {
    let logs = CapturedLogs::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        // CI colourizes by default, and the escape codes land between the span
        // name and its brace, which breaks the CLOSE-line matcher below. Locally
        // this is a no-op because there is no TTY.
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();

    let upstream = start_echo_upstream().await;
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let proxy = start_proxy(&upstream, FrameLogger::disabled()).await;

        let (mut client, _) = tokio_tungstenite::connect_async(frameless_request(format!(
            "{proxy}/v1/live/rtc_span"
        )))
        .await
        .expect("upgrade");
        client
            .send(ClientMessage::Text("ping".into()))
            .await
            .unwrap();
        let _ = client.next().await;
        drop(client);

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    let text = logs.text();
    let span_line = logs.close_line("sideband");
    assert!(
        !span_line.is_empty(),
        "no sideband span CLOSE line was emitted, so `.instrument(span)` may be missing: {text}"
    );
    for expected in [
        "join_style=\"frameless_path\"",
        "upstream=127.0.0.1:",
        "outcome=\"client_closed\"",
        "code=1000",
    ] {
        assert!(
            span_line.contains(expected),
            "the span itself is missing {expected}: {span_line}"
        );
    }
    assert!(
        !text.contains("rtc_span"),
        "the call id must not be logged: {text}"
    );
}
