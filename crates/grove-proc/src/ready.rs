use std::future::Future;
use std::net::Ipv4Addr;
use std::time::Duration;

use grove_config::{ReadyCheck, ReadyKind};
use regex::bytes::{Regex, RegexBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::exit::ExitInfo;
use crate::process::{ManagedProcess, OutputSubscription};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Dev servers often compile a route on its first request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_HEAD: usize = 8 * 1024;
/// Longest unfinished line kept while waiting for the rest of it.
const MAX_PENDING_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadyError {
    #[error("not ready before the timeout")]
    Timeout,
    #[error("exited before it was ready ({0})")]
    Exited(ExitInfo),
    #[error("the readiness check needs a port, but the process has none")]
    NoPort,
    #[error("invalid `ready.log` pattern: {0}")]
    InvalidPattern(String),
}

/// Waits until `check` passes, polling about every 250ms. Fails fast if the
/// process exits first.
///
/// - Http: `GET http://127.0.0.1:{port}{path}` answers 2xx or 3xx.
/// - Tcp: `127.0.0.1:{port}` accepts a connection.
/// - Log: the ANSI-stripped output matches the regex, starting with the
///   replay. `^` and `$` match at line boundaries.
pub async fn wait_ready(
    check: &ReadyCheck,
    port: Option<u16>,
    process: &ManagedProcess,
) -> Result<(), ReadyError> {
    let deadline = Instant::now() + check.timeout;
    let probe = async {
        match &check.kind {
            ReadyKind::Http { path } => {
                let port = port.ok_or(ReadyError::NoPort)?;
                poll(|| http_ok(port, path)).await;
            }
            ReadyKind::Tcp => {
                let port = port.ok_or(ReadyError::NoPort)?;
                poll(|| tcp_ok(port)).await;
            }
            ReadyKind::Log { pattern } => {
                let regex = RegexBuilder::new(pattern)
                    .multi_line(true)
                    .build()
                    .map_err(|e| ReadyError::InvalidPattern(e.to_string()))?;
                wait_for_log(&regex, process).await;
            }
        }
        Ok(())
    };
    tokio::select! {
        biased;
        result = probe => result,
        info = process.wait() => Err(ReadyError::Exited(info)),
        () = sleep_until(deadline) => Err(ReadyError::Timeout),
    }
}

async fn poll<F, Fut>(mut attempt: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    while !attempt().await {
        sleep(POLL_INTERVAL).await;
    }
}

async fn connect(port: u16) -> Option<TcpStream> {
    timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .ok()?
    .ok()
}

async fn tcp_ok(port: u16) -> bool {
    connect(port).await.is_some()
}

async fn http_ok(port: u16, path: &str) -> bool {
    let status = timeout(HTTP_TIMEOUT, http_status(port, path)).await;
    status
        .ok()
        .flatten()
        .is_some_and(|code| (200..400).contains(&code))
}

async fn http_status(port: u16, path: &str) -> Option<u16> {
    let mut stream = connect(port).await?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: grove\r\n\
         Accept: */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut head = Vec::with_capacity(512);
    while !head.contains(&b'\n') && head.len() < MAX_RESPONSE_HEAD {
        if stream.read_buf(&mut head).await.ok()? == 0 {
            break;
        }
    }
    parse_status_line(&head)
}

fn parse_status_line(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\n').next()?;
    let mut parts = std::str::from_utf8(line).ok()?.split_whitespace();
    if !parts.next()?.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Returns once the output matches; never returns if the output ends first
/// (the caller's exit and deadline branches decide then).
async fn wait_for_log(regex: &Regex, process: &ManagedProcess) {
    loop {
        let OutputSubscription { replay, mut rx } = process.subscribe();
        let mut matcher = LineMatcher::new(regex);
        if matcher.feed(&replay) {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    if matcher.feed(&chunk) {
                        return;
                    }
                }
                // Chunks were dropped; they may still be in a fresh replay.
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => return std::future::pending().await,
            }
        }
    }
}

/// Matches a regex against output that arrives in arbitrary chunks. The raw
/// bytes of the unfinished last line are kept and re-stripped together with
/// the next chunk, so both matches and escape sequences may span chunks.
struct LineMatcher<'a> {
    regex: &'a Regex,
    pending: Vec<u8>,
}

impl<'a> LineMatcher<'a> {
    fn new(regex: &'a Regex) -> Self {
        Self {
            regex,
            pending: Vec::new(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> bool {
        self.pending.extend_from_slice(chunk);
        let matched = self
            .regex
            .is_match(&strip_ansi_escapes::strip(&self.pending));
        if let Some(i) = self.pending.iter().rposition(|b| *b == b'\n') {
            self.pending.drain(..=i);
        }
        if self.pending.len() > MAX_PENDING_LINE {
            let excess = self.pending.len() - MAX_PENDING_LINE;
            self.pending.drain(..excess);
        }
        matched
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant as StdInstant;

    use tokio::net::TcpListener;

    use super::*;
    use crate::process::tests::{Guard, output_until, sh, spawn};

    fn check(kind: ReadyKind, millis: u64) -> ReadyCheck {
        ReadyCheck {
            kind,
            timeout: Duration::from_millis(millis),
        }
    }

    fn log(pattern: &str) -> ReadyKind {
        ReadyKind::Log {
            pattern: pattern.into(),
        }
    }

    fn sleeper() -> Guard {
        spawn(sh("sleep 10"))
    }

    fn regex(pattern: &str) -> Regex {
        RegexBuilder::new(pattern).multi_line(true).build().unwrap()
    }

    #[test]
    fn matches_across_chunks_and_escapes() {
        let re = regex(r"^server ready on port \d+$");
        let mut m = LineMatcher::new(&re);
        assert!(!m.feed(b"booting\r\n\x1b[3"));
        assert!(!m.feed(b"2mserver rea"));
        assert!(m.feed(b"dy on port 3000\x1b[0m\r\n"));
    }

    #[test]
    fn anchors_are_per_line() {
        let re = regex(r"^ready$");
        let mut m = LineMatcher::new(&re);
        assert!(!m.feed(b"not ready\r\n"));
        assert!(!m.feed(b"ready or not\r\n"));
        assert!(m.feed(b"ready\r\n"));
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status_line(b"HTTP/1.1 204 No Content\r\n"), Some(204));
        assert_eq!(parse_status_line(b"HTTP/1.0 302\r\n"), Some(302));
        assert_eq!(parse_status_line(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(parse_status_line(b""), None);
    }

    #[tokio::test]
    async fn tcp_ready() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let p = sleeper();
        wait_ready(&check(ReadyKind::Tcp, 5000), Some(port), &p.0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tcp_times_out() {
        let port = {
            let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let p = sleeper();
        let err = wait_ready(&check(ReadyKind::Tcp, 600), Some(port), &p.0).await;
        assert_eq!(err, Err(ReadyError::Timeout));
    }

    #[tokio::test]
    async fn http_ready_after_retries() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in ["503 Service Unavailable", "200 OK"] {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
                sock.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let p = sleeper();
        let http = ReadyKind::Http {
            path: "/health".into(),
        };
        wait_ready(&check(http, 5000), Some(port), &p.0)
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /health HTTP/1.1\r\n"));
        assert!(requests[1].contains(&format!("Host: 127.0.0.1:{port}\r\n")));
    }

    #[tokio::test]
    async fn log_ready_from_the_stream() {
        let p = spawn(sh(
            r"printf 'server rea'; sleep 0.3; printf 'dy on port 3000\n'; sleep 10",
        ));
        let started = StdInstant::now();
        wait_ready(&check(log(r"ready on port \d+"), 5000), None, &p.0)
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[tokio::test]
    async fn log_ready_from_the_replay() {
        let p = spawn(sh("printf '\\033[32mbooted\\033[0m\\n'; sleep 10"));
        output_until(&p.0, "booted").await;
        wait_ready(&check(log(r"^booted$"), 1000), None, &p.0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exit_fails_fast() {
        let p = spawn(sh("sleep 0.2; exit 3"));
        let started = StdInstant::now();
        let http = ReadyKind::Http { path: "/".into() };
        let err = wait_ready(&check(http, 30_000), Some(1), &p.0).await;
        match err {
            Err(ReadyError::Exited(info)) => assert_eq!(info.code, Some(3)),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn log_times_out() {
        let p = sleeper();
        let err = wait_ready(&check(log("never"), 300), None, &p.0).await;
        assert_eq!(err, Err(ReadyError::Timeout));
    }

    #[tokio::test]
    async fn needs_a_port() {
        let p = sleeper();
        let err = wait_ready(&check(ReadyKind::Tcp, 1000), None, &p.0).await;
        assert_eq!(err, Err(ReadyError::NoPort));
    }

    #[tokio::test]
    async fn bad_pattern() {
        let p = sleeper();
        let err = wait_ready(&check(log("("), 1000), None, &p.0).await;
        assert!(matches!(err, Err(ReadyError::InvalidPattern(_))));
    }
}
