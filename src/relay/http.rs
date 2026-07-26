//! Shared lifecycle and execution for bounded HTTP relay exchanges.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, StatusCode};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::RelayError;

/// A fully buffered upstream response. Protocol owners decide which headers
/// are safe to expose downstream.
pub struct OpaqueResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// The first terminal observation for one HTTP exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeTerminal {
    InFlight,
    Completed,
    Failed,
    TimedOut,
    Canceled,
}

#[derive(Debug)]
struct ExchangeShared {
    terminal: Mutex<ExchangeTerminal>,
    cancellation: CancellationToken,
}

/// Cloneable access to the exchange's first-writer terminal slot.
#[derive(Debug, Clone)]
pub struct ExchangeLifecycle(Arc<ExchangeShared>);

impl ExchangeLifecycle {
    /// Records `next` only while the exchange is still in flight.
    pub fn finish(&self, next: ExchangeTerminal) -> bool {
        if next == ExchangeTerminal::InFlight {
            return false;
        }
        let mut terminal = self
            .0
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *terminal != ExchangeTerminal::InFlight {
            return false;
        }
        *terminal = next;
        true
    }

    pub fn terminal(&self) -> ExchangeTerminal {
        *self
            .0
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Handler-owned cancellation guard. A completed task wins over a later drop.
pub struct ExchangeGuard {
    lifecycle: ExchangeLifecycle,
}

impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        if self.lifecycle.finish(ExchangeTerminal::Canceled) {
            self.lifecycle.0.cancellation.cancel();
        }
    }
}

pub fn begin_exchange() -> (ExchangeLifecycle, ExchangeGuard) {
    let lifecycle = ExchangeLifecycle(Arc::new(ExchangeShared {
        terminal: Mutex::new(ExchangeTerminal::InFlight),
        cancellation: CancellationToken::new(),
    }));
    let guard = ExchangeGuard {
        lifecycle: lifecycle.clone(),
    };
    (lifecycle, guard)
}

/// Spawns the whole upstream send/read lifecycle. The owned permit remains in
/// the task until its terminal state has been recorded and the task exits.
pub fn spawn_execute(
    client: reqwest::Client,
    request: reqwest::Request,
    response_cap: usize,
    upstream_timeout: Duration,
    lifecycle: ExchangeLifecycle,
    permit: OwnedSemaphorePermit,
) -> JoinHandle<Result<OpaqueResponse, RelayError>> {
    spawn_execute_with_after_finish(
        client,
        request,
        response_cap,
        upstream_timeout,
        lifecycle,
        permit,
        || async {},
    )
}

fn spawn_execute_with_after_finish<F, Fut>(
    client: reqwest::Client,
    request: reqwest::Request,
    response_cap: usize,
    upstream_timeout: Duration,
    lifecycle: ExchangeLifecycle,
    permit: OwnedSemaphorePermit,
    after_finish: F,
) -> JoinHandle<Result<OpaqueResponse, RelayError>>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let _permit = permit;
        let cancellation = lifecycle.0.cancellation.clone();
        let work = execute_capped(client, request, response_cap);

        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(RelayError::ClientCanceled),
            result = tokio::time::timeout(upstream_timeout, work) => match result {
                Ok(result) => result,
                Err(_) => Err(RelayError::UpstreamTimeout),
            },
        };

        let terminal = match &result {
            Ok(_) => ExchangeTerminal::Completed,
            Err(RelayError::UpstreamTimeout) => ExchangeTerminal::TimedOut,
            Err(RelayError::ClientCanceled) => ExchangeTerminal::Canceled,
            Err(_) => ExchangeTerminal::Failed,
        };
        lifecycle.finish(terminal);
        // The terminal write deliberately precedes result exposure. Keeping
        // this seam explicit lets the race test pause at that exact boundary.
        after_finish().await;
        result
    })
}

async fn execute_capped(
    client: reqwest::Client,
    request: reqwest::Request,
    response_cap: usize,
) -> Result<OpaqueResponse, RelayError> {
    let mut response = client
        .execute(request)
        .await
        .map_err(|err| RelayError::UpstreamFailed(err.to_string()))?;
    let status = response.status();
    let headers = response.headers().clone();
    let mut body = BytesMut::new();

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| RelayError::UpstreamFailed(err.to_string()))?
    {
        if chunk.is_empty() {
            continue;
        }
        let observed = body.len().saturating_add(chunk.len());
        if observed > response_cap {
            return Err(RelayError::ResponseTooLarge(observed));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(OpaqueResponse {
        status,
        headers,
        body: body.freeze(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;

    use super::*;

    #[test]
    fn completion_before_guard_drop_stays_completed() {
        let (lifecycle, guard) = begin_exchange();
        assert!(lifecycle.finish(ExchangeTerminal::Completed));
        drop(guard);
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        assert!(!lifecycle.0.cancellation.is_cancelled());
    }

    #[test]
    fn guard_drop_before_completion_stays_canceled() {
        let (lifecycle, guard) = begin_exchange();
        drop(guard);
        assert!(!lifecycle.finish(ExchangeTerminal::Completed));
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Canceled);
        assert!(lifecycle.0.cancellation.is_cancelled());
    }

    async fn one_response(body: &'static [u8]) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.expect("read request");
                assert_ne!(read, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
            let headers = format!(
                "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.expect("headers");
            stream.write_all(body).await.expect("body");
        });
        (format!("http://{address}/exchange"), request_rx)
    }

    async fn hanging_response() -> (String, tokio::sync::oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            let _ = accepted_tx.send(());
            std::future::pending::<()>().await;
        });
        (format!("http://{address}/stall"), accepted_rx)
    }

    #[tokio::test]
    async fn arbitrary_method_completes_before_join_returns_and_releases_permit() {
        let (url, request_rx) = one_response(b"opaque").await;
        let client = reqwest::Client::new();
        let request = client
            .request(reqwest::Method::PATCH, url)
            .body("request")
            .build()
            .expect("request");
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().expect("permit");
        let (lifecycle, guard) = begin_exchange();

        let result = spawn_execute(
            client,
            request,
            6,
            Duration::from_secs(1),
            lifecycle.clone(),
            permit,
        )
        .await
        .expect("task")
        .expect("exchange");

        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        drop(guard);
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        assert_eq!(result.status, StatusCode::MULTI_STATUS);
        assert_eq!(result.body, Bytes::from_static(b"opaque"));
        assert_eq!(
            result.headers.get(http::header::CONTENT_TYPE),
            Some(&http::HeaderValue::from_static("application/octet-stream"))
        );
        assert!(request_rx
            .await
            .expect("request line")
            .starts_with("PATCH /exchange "));
        assert!(
            permits.try_acquire().is_ok(),
            "task must release its permit"
        );
    }

    #[tokio::test]
    async fn response_cap_failure_is_terminal_before_join_returns() {
        let (url, _request_rx) = one_response(b"seven!!").await;
        let client = reqwest::Client::new();
        let request = client.get(url).build().expect("request");
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().expect("permit");
        let (lifecycle, _guard) = begin_exchange();

        let result = spawn_execute(
            client,
            request,
            6,
            Duration::from_secs(1),
            lifecycle.clone(),
            permit,
        )
        .await
        .expect("task");
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("response must exceed cap"),
        };

        assert!(matches!(err, RelayError::ResponseTooLarge(7)));
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Failed);
        assert!(
            permits.try_acquire().is_ok(),
            "task must release its permit"
        );
    }

    #[tokio::test]
    async fn stalled_response_records_timeout_before_join_returns() {
        let (url, accepted_rx) = hanging_response().await;
        let client = reqwest::Client::new();
        let request = client.get(url).build().expect("request");
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().expect("permit");
        let (lifecycle, _guard) = begin_exchange();

        let task = spawn_execute(
            client,
            request,
            1,
            Duration::from_millis(100),
            lifecycle.clone(),
            permit,
        );
        accepted_rx.await.expect("upstream accepted request");
        let result = task.await.expect("task");
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("stalled response must time out"),
        };

        assert!(matches!(err, RelayError::UpstreamTimeout));
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::TimedOut);
        assert!(
            permits.try_acquire().is_ok(),
            "task must release its permit"
        );
    }

    #[tokio::test]
    async fn guard_drop_cancels_spawned_work_and_releases_permit() {
        let (url, accepted_rx) = hanging_response().await;
        let client = reqwest::Client::new();
        let request = client.get(url).build().expect("request");
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().expect("permit");
        let (lifecycle, guard) = begin_exchange();
        let task = spawn_execute(
            client,
            request,
            1,
            Duration::from_secs(5),
            lifecycle.clone(),
            permit,
        );
        accepted_rx.await.expect("upstream accepted request");

        drop(guard);
        let result = task.await.expect("task");
        assert!(matches!(result, Err(RelayError::ClientCanceled)));
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Canceled);
        assert!(
            permits.try_acquire().is_ok(),
            "task must release its permit"
        );
    }

    #[tokio::test]
    async fn completion_wins_if_the_handler_drops_before_task_return() {
        let (url, _request_rx) = one_response(b"done").await;
        let client = reqwest::Client::new();
        let request = client.get(url).build().expect("request");
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().expect("permit");
        let (lifecycle, guard) = begin_exchange();
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = spawn_execute_with_after_finish(
            client,
            request,
            4,
            Duration::from_secs(1),
            lifecycle.clone(),
            permit,
            {
                let reached = reached.clone();
                let release = release.clone();
                move || async move {
                    reached.notify_one();
                    release.notified().await;
                }
            },
        );

        reached.notified().await;
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        drop(guard);
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        assert_eq!(
            permits.available_permits(),
            0,
            "the spawned task must retain its permit until it exits"
        );

        release.notify_one();
        let result = task.await.expect("task").expect("exchange");
        assert_eq!(result.body, Bytes::from_static(b"done"));
        assert_eq!(lifecycle.terminal(), ExchangeTerminal::Completed);
        assert_eq!(permits.available_permits(), 1);
    }
}
