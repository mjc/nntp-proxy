//! Minimal HTTP/1.1 responder for `GET /metrics`.
//!
//! A scrape endpoint needs one route and no keep-alive, so a few dozen lines
//! on top of tokio replace a web framework. Every connection is bounded: the
//! request head has a size cap and a read deadline, the response a write
//! deadline, and connections beyond the concurrency limit are closed at once
//! rather than queued, so scrapers can never pile work onto the proxy.

use super::source::ScrapeSource;
use crate::constants::metrics as limits;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

/// Content type for text exposition format 0.0.4.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Connection limits for the scrape endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    pub max_request_head_bytes: usize,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_concurrent: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_request_head_bytes: limits::MAX_REQUEST_HEAD_BYTES,
            read_timeout: limits::READ_TIMEOUT,
            write_timeout: limits::WRITE_TIMEOUT,
            max_concurrent: limits::MAX_CONCURRENT_SCRAPES,
        }
    }
}

/// What the request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `GET /metrics`, or `HEAD /metrics` (headers only).
    Metrics {
        body: ResponseBody,
    },
    NotFound {
        body: ResponseBody,
    },
    MethodNotAllowed,
    BadRequest,
}

/// Whether the response carries a body; `HEAD` responses must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseBody {
    Include,
    Omit,
}

/// Why a request head could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadError {
    TooLarge,
    Closed,
}

/// Bind the scrape listener.
///
/// # Errors
/// Returns the bind error, so a misconfigured address fails startup instead of
/// silently leaving the proxy unmonitored.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Serve scrapes on `listener` until the task is dropped or aborted.
pub async fn serve<S: ScrapeSource>(listener: TcpListener, source: Arc<S>, limits: ServerLimits) {
    let slots = Arc::new(Semaphore::new(limits.max_concurrent));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // Usually descriptor exhaustion; back off briefly instead of
                // spinning on an error the next accept will repeat.
                debug!(error = %error, "Metrics listener accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
            debug!(%peer, "Metrics endpoint busy; closing scrape connection");
            drop(stream);
            continue;
        };
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_connection(stream, source, limits).await {
                debug!(%peer, error = %error, "Metrics scrape connection ended with an error");
            }
        });
    }
}

async fn handle_connection<S: ScrapeSource>(
    mut stream: TcpStream,
    source: Arc<S>,
    limits: ServerLimits,
) -> std::io::Result<()> {
    let mut head = vec![0u8; limits.max_request_head_bytes];
    let route = match tokio::time::timeout(
        limits.read_timeout,
        read_request_head(&mut stream, &mut head),
    )
    .await
    {
        Err(_elapsed) => return Ok(()),
        Ok(Err(HeadError::Closed)) => return Ok(()),
        Ok(Err(HeadError::TooLarge)) => {
            return write_with_timeout(
                &mut stream,
                &simple_response("431 Request Header Fields Too Large", ResponseBody::Include),
                limits.write_timeout,
            )
            .await;
        }
        Ok(Ok(len)) => route(&head[..len]),
    };
    drop(head);

    let response = match route {
        // HEAD is deliberately a listener check, not a render check:
        // rendering would read every collector for a body that is never sent.
        // A monitor that wants proof a scrape works should use GET.
        Route::Metrics {
            body: ResponseBody::Omit,
        } => metrics_head_response(),
        Route::Metrics {
            body: ResponseBody::Include,
        } => {
            let rendered = match tokio::task::spawn_blocking(move || source.render()).await {
                Ok(rendered) => rendered,
                Err(error) => {
                    warn!(error = %error, "Rendering metrics failed");
                    let response =
                        simple_response("500 Internal Server Error", ResponseBody::Include);
                    return write_with_timeout(&mut stream, &response, limits.write_timeout).await;
                }
            };
            metrics_response(&rendered)
        }
        Route::NotFound { body } => simple_response("404 Not Found", body),
        Route::MethodNotAllowed => simple_response("405 Method Not Allowed", ResponseBody::Include),
        Route::BadRequest => simple_response("400 Bad Request", ResponseBody::Include),
    };
    write_with_timeout(&mut stream, &response, limits.write_timeout).await
}

/// Read until the blank line that ends the request head; returns its length.
async fn read_request_head(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, HeadError> {
    let mut filled = 0;
    loop {
        if filled == buf.len() {
            return Err(HeadError::TooLarge);
        }
        let n = stream
            .read(&mut buf[filled..])
            .await
            .map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(HeadError::Closed);
        }
        // Rescan a few bytes before the new data in case the blank line
        // straddles two reads.
        let scan_from = filled.saturating_sub(3);
        filled += n;
        if let Some(pos) = memchr::memmem::find(&buf[scan_from..filled], b"\r\n\r\n") {
            return Ok(scan_from + pos + 4);
        }
        if let Some(pos) = memchr::memmem::find(&buf[scan_from..filled], b"\n\n") {
            return Ok(scan_from + pos + 2);
        }
    }
}

/// Decide the response from the request line alone; headers are ignored.
fn route(head: &[u8]) -> Route {
    let Ok(head) = std::str::from_utf8(head) else {
        return Route::BadRequest;
    };
    let Some(request_line) = head.lines().next() else {
        return Route::BadRequest;
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Route::BadRequest;
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Route::BadRequest;
    }
    let body = match method {
        "GET" => ResponseBody::Include,
        "HEAD" => ResponseBody::Omit,
        _ => return Route::MethodNotAllowed,
    };
    let path = target.split_once('?').map_or(target, |(path, _query)| path);
    if path == "/metrics" {
        Route::Metrics { body }
    } else {
        Route::NotFound { body }
    }
}

fn metrics_response(rendered: &str) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {CONTENT_TYPE}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        rendered.len()
    );
    let mut response = Vec::with_capacity(header.len() + rendered.len());
    response.extend_from_slice(header.as_bytes());
    response.extend_from_slice(rendered.as_bytes());
    response
}

/// Headers for `HEAD /metrics`. No `Content-Length`: the body length is
/// unknown without rendering, and HTTP lets a HEAD response omit it.
fn metrics_head_response() -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {CONTENT_TYPE}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

fn simple_response(status: &str, body: ResponseBody) -> Vec<u8> {
    let text = format!("{status}\n");
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
        text.len()
    );
    if status.split(' ').next() == Some("405") {
        response.push_str("Allow: GET, HEAD\r\n");
    }
    response.push_str("\r\n");
    if body == ResponseBody::Include {
        response.push_str(&text);
    }
    response.into_bytes()
}

async fn write_with_timeout(
    stream: &mut TcpStream,
    response: &[u8],
    write_timeout: Duration,
) -> std::io::Result<()> {
    let write = async {
        stream.write_all(response).await?;
        stream.shutdown().await
    };
    tokio::time::timeout(write_timeout, write)
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "metrics response write timed out",
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource(&'static str);

    impl ScrapeSource for FixedSource {
        fn render(&self) -> String {
            self.0.to_string()
        }
    }

    async fn start(limits: ServerLimits) -> SocketAddr {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(
            listener,
            Arc::new(FixedSource("nntp_proxy_up 1\n")),
            limits,
        ));
        addr
    }

    async fn exchange(addr: SocketAddr, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = Vec::new();
        read_until_closed(&mut stream, &mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    /// Read until the server closes the connection. The project bans
    /// `read_to_end` so NNTP paths cannot bypass framing; HTTP test reads
    /// loop explicitly instead.
    async fn read_until_closed(
        stream: &mut TcpStream,
        out: &mut Vec<u8>,
    ) -> std::io::Result<usize> {
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(out.len());
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    fn status_line(response: &str) -> &str {
        response.lines().next().unwrap_or_default()
    }

    #[test]
    fn route_matches_only_get_or_head_metrics() {
        assert_eq!(
            route(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n"),
            Route::Metrics {
                body: ResponseBody::Include
            }
        );
        assert_eq!(
            route(b"GET /metrics?x=1 HTTP/1.0\r\n\r\n"),
            Route::Metrics {
                body: ResponseBody::Include
            }
        );
        assert_eq!(
            route(b"HEAD /metrics HTTP/1.1\r\n\r\n"),
            Route::Metrics {
                body: ResponseBody::Omit
            }
        );
        assert_eq!(
            route(b"GET / HTTP/1.1\r\n\r\n"),
            Route::NotFound {
                body: ResponseBody::Include
            }
        );
        assert_eq!(
            route(b"GET /metricsx HTTP/1.1\r\n\r\n"),
            Route::NotFound {
                body: ResponseBody::Include
            }
        );
        assert_eq!(
            route(b"POST /metrics HTTP/1.1\r\n\r\n"),
            Route::MethodNotAllowed
        );
        assert_eq!(route(b"GET /metrics\r\n\r\n"), Route::BadRequest);
        assert_eq!(route(b"GET /metrics SPDY/3\r\n\r\n"), Route::BadRequest);
        assert_eq!(route(b"\xff\xfe\r\n\r\n"), Route::BadRequest);
    }

    #[tokio::test]
    async fn get_metrics_returns_exposition_with_content_type() {
        let addr = start(ServerLimits::default()).await;
        let response = exchange(addr, b"GET /metrics HTTP/1.1\r\nHost: test\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
        assert!(response.contains(&format!("Content-Type: {CONTENT_TYPE}\r\n")));
        assert!(response.contains("Content-Length: 16\r\n"));
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        assert_eq!(body, "nntp_proxy_up 1\n");
    }

    #[tokio::test]
    async fn head_metrics_omits_body() {
        let addr = start(ServerLimits::default()).await;
        let response = exchange(addr, b"HEAD /metrics HTTP/1.1\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn head_metrics_does_not_render() {
        struct CountingSource(std::sync::atomic::AtomicUsize);
        impl ScrapeSource for CountingSource {
            fn render(&self) -> String {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                String::new()
            }
        }
        let source = Arc::new(CountingSource(std::sync::atomic::AtomicUsize::new(0)));
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(
            listener,
            Arc::clone(&source),
            ServerLimits::default(),
        ));

        let response = exchange(addr, b"HEAD /metrics HTTP/1.1\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
        assert_eq!(source.0.load(std::sync::atomic::Ordering::Relaxed), 0);
        let _ = exchange(addr, b"GET /metrics HTTP/1.1\r\n\r\n").await;
        assert_eq!(source.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn other_paths_are_not_found() {
        let addr = start(ServerLimits::default()).await;
        let response = exchange(addr, b"GET /debug HTTP/1.1\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 404 Not Found");
        let response = exchange(addr, b"DELETE /metrics HTTP/1.1\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 405 Method Not Allowed");
        assert!(response.contains("Allow: GET, HEAD\r\n"));
    }

    #[tokio::test]
    async fn request_head_split_across_writes_is_reassembled() {
        let addr = start(ServerLimits::default()).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\n\r")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        stream.write_all(b"\n").await.unwrap();
        let mut response = Vec::new();
        read_until_closed(&mut stream, &mut response).await.unwrap();
        assert_eq!(
            status_line(&String::from_utf8(response).unwrap()),
            "HTTP/1.1 200 OK"
        );
    }

    #[tokio::test]
    async fn oversized_request_head_is_rejected() {
        let addr = start(ServerLimits {
            max_request_head_bytes: 256,
            ..ServerLimits::default()
        })
        .await;
        let mut request = b"GET /metrics HTTP/1.1\r\nX-Pad: ".to_vec();
        request.extend(std::iter::repeat_n(b'a', 1024));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        // The server may close before reading everything; a write error then
        // is expected, the status line is what matters.
        let _ = stream.write_all(&request).await;
        let mut response = Vec::new();
        let _ = read_until_closed(&mut stream, &mut response).await;
        let response = String::from_utf8_lossy(&response);
        assert_eq!(
            status_line(&response),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
    }

    #[tokio::test]
    async fn slow_client_is_disconnected_after_read_timeout() {
        let addr = start(ServerLimits {
            read_timeout: Duration::from_millis(100),
            ..ServerLimits::default()
        })
        .await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET /metr").await.unwrap();
        let mut response = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            read_until_closed(&mut stream, &mut response),
        )
        .await
        .expect("server closes a stalled scrape well before the test deadline");
        assert!(read.is_err() || response.is_empty());
    }

    #[tokio::test]
    async fn connections_over_the_limit_are_closed_without_blocking_others() {
        let addr = start(ServerLimits {
            max_concurrent: 1,
            read_timeout: Duration::from_millis(300),
            ..ServerLimits::default()
        })
        .await;
        // Hold the only slot with a stalled request.
        let mut holder = TcpStream::connect(addr).await.unwrap();
        holder.write_all(b"GET").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut extra = TcpStream::connect(addr).await.unwrap();
        let _ = extra.write_all(b"GET /metrics HTTP/1.1\r\n\r\n").await;
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            read_until_closed(&mut extra, &mut response),
        )
        .await
        .expect("over-limit connection is closed promptly");
        assert!(response.is_empty());

        // Once the stalled request times out, the slot is free again.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let response = exchange(addr, b"GET /metrics HTTP/1.1\r\n\r\n").await;
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
    }
}
