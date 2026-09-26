//! Listeners, connection serving, and the proxy's lifecycle.

use crate::backend::Backend;
use crate::body::Body;
use crate::ca::CertAuthority;
use crate::command;
use crate::error::ProxyError;
use crate::forward::{self, ConnInfo};
use crate::handler::{self, Shared};
use crate::routes::{Route, RouteTable};
use crate::tls;
use http::{Request, Response};
use hyper::body::Incoming;
use hyper::service::{Service, service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use parking_lot::RwLock;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long in-flight requests get to finish on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub https_addrs: Vec<SocketAddr>,
    /// Plain HTTP listeners; they redirect everything to HTTPS.
    pub http_addrs: Vec<SocketAddr>,
    /// How long a non-navigation request to a starting cluster is held.
    pub ready_timeout: Duration,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        let loopback = |port| {
            vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ]
        };
        Self {
            https_addrs: loopback(443),
            http_addrs: loopback(80),
            ready_timeout: Duration::from_secs(120),
        }
    }
}

/// A running proxy. Dropping it stops it too, without waiting.
pub struct Proxy {
    shared: Arc<Shared>,
    https_addrs: Vec<SocketAddr>,
    http_addrs: Vec<SocketAddr>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Proxy {
    /// Binds every listener, then serves in background tasks. Starts with no
    /// routes.
    pub async fn start(
        config: ProxyConfig,
        ca: Arc<CertAuthority>,
        backend: Arc<dyn Backend>,
    ) -> Result<Proxy, ProxyError> {
        let acceptor = TlsAcceptor::from(Arc::new(tls::server_config(ca)?));
        let https = bind_all(&config.https_addrs).await?;
        let http = bind_all(&config.http_addrs).await?;
        let https_addrs = local_addrs(&https);
        let http_addrs = local_addrs(&http);
        let https_port = https_addrs.first().map_or(443, SocketAddr::port);

        let (shutdown, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            routes: RwLock::new(Arc::new(RouteTable::default())),
            backend,
            client: forward::upstream_client(),
            ready_timeout: config.ready_timeout,
            shutdown: shutdown_rx.clone(),
        });

        let mut tasks = Vec::new();
        for listener in https {
            let serve = serve_https(
                listener,
                acceptor.clone(),
                shared.clone(),
                shutdown_rx.clone(),
            );
            tasks.push(tokio::spawn(serve));
        }
        for listener in http {
            tasks.push(tokio::spawn(serve_http(
                listener,
                https_port,
                shutdown_rx.clone(),
            )));
        }

        Ok(Proxy {
            shared,
            https_addrs,
            http_addrs,
            shutdown,
            tasks,
        })
    }

    /// Replaces the whole routing table. Requests already in flight keep the
    /// route they matched.
    pub fn set_routes(&self, routes: Vec<Route>) {
        *self.shared.routes.write() = Arc::new(RouteTable::new(routes));
    }

    /// `(https, http)` addresses actually bound, e.g. to learn the ports when
    /// configured with port 0.
    pub fn local_addrs(&self) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        (self.https_addrs.clone(), self.http_addrs.clone())
    }

    /// Stops accepting, gives in-flight requests a moment to finish, then
    /// closes every connection, including upgraded ones.
    pub async fn shutdown(mut self) {
        self.shutdown.send_replace(true);
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
    }
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("https_addrs", &self.https_addrs)
            .field("http_addrs", &self.http_addrs)
            .finish_non_exhaustive()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

async fn bind_all(addrs: &[SocketAddr]) -> Result<Vec<TcpListener>, ProxyError> {
    let mut listeners = Vec::with_capacity(addrs.len());
    for &addr in addrs {
        match TcpListener::bind(addr).await {
            Ok(listener) => listeners.push(listener),
            Err(e) if addr.is_ipv6() && e.kind() == io::ErrorKind::AddrNotAvailable => {
                tracing::warn!(%addr, "IPv6 loopback unavailable, not listening on it: {e}");
            }
            Err(source) => {
                let port = addr.port();
                let owner = tokio::task::spawn_blocking(move || port_owner(port))
                    .await
                    .ok()
                    .flatten();
                return Err(ProxyError::Bind {
                    addr,
                    source,
                    owner,
                });
            }
        }
    }
    Ok(listeners)
}

fn local_addrs(listeners: &[TcpListener]) -> Vec<SocketAddr> {
    listeners
        .iter()
        .filter_map(|l| l.local_addr().ok())
        .collect()
}

/// The process listening on `port`, e.g. `caddy (pid 812)`. Best effort:
/// `lsof` can't see other users' processes without root.
fn port_owner(port: u16) -> Option<String> {
    let out = command::run(
        Command::new("/usr/sbin/lsof").args([
            "-nP",
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "-Fpc",
        ]),
        Duration::from_secs(5),
    )
    .ok()?;
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

/// Parses `lsof -F pc` output: `p<pid>` and `c<command>` lines per process.
fn parse_lsof(output: &str) -> Option<String> {
    let mut pid = None;
    let mut owners: Vec<String> = Vec::new();
    for line in output.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = Some(p);
        } else if let (Some(name), Some(pid)) = (line.strip_prefix('c'), pid) {
            let owner = format!("{name} (pid {pid})");
            if !owners.contains(&owner) {
                owners.push(owner);
            }
        }
    }
    (!owners.is_empty()).then(|| owners.join(", "))
}

async fn serve_https(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
) {
    let stop = shutdown.clone();
    accept_loop(listener, shutdown, move |stream, conn| {
        https_connection(stream, conn, acceptor.clone(), shared.clone(), stop.clone())
    })
    .await;
}

async fn https_connection(
    stream: TcpStream,
    conn: ConnInfo,
    acceptor: TlsAcceptor,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
) {
    let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            tracing::debug!(peer = %conn.peer, "TLS handshake failed: {e}");
            return;
        }
        Err(_) => {
            tracing::debug!(peer = %conn.peer, "TLS handshake timed out");
            return;
        }
    };
    let service = service_fn(move |req| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handler::handle(shared, conn, req).await) }
    });
    serve_connection(stream, service, shutdown).await;
}

/// Plain HTTP only redirects to HTTPS.
async fn serve_http(listener: TcpListener, https_port: u16, shutdown: watch::Receiver<bool>) {
    let stop = shutdown.clone();
    accept_loop(listener, shutdown, move |stream, _| {
        let service = service_fn(move |req: Request<Incoming>| async move {
            Ok::<_, Infallible>(handler::redirect_to_https(&req, https_port))
        });
        serve_connection(stream, service, stop.clone())
    })
    .await;
}

/// Accepts connections until shutdown, running `serve` for each on its own
/// task. On shutdown, waits briefly for connections to wind down, then
/// aborts the rest.
async fn accept_loop<F, Fut>(listener: TcpListener, mut shutdown: watch::Receiver<bool>, serve: F)
where
    F: Fn(TcpStream, ConnInfo) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let Ok(local) = listener.local_addr() else {
        return;
    };
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped(&mut shutdown) => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    connections.spawn(serve(stream, ConnInfo { peer, local }));
                }
                Err(e) => {
                    // Usually out of file descriptors; don't spin.
                    tracing::warn!(%local, "accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    let drain = async { while connections.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(SHUTDOWN_GRACE * 2, drain).await;
}

/// Resolves once the proxy is shutting down.
pub(crate) async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn serve_connection<I, S>(io: I, service: S, mut shutdown: watch::Receiver<bool>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Service<Request<Incoming>, Response = Response<Body>, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new());
    builder.http2().timer(TokioTimer::new());
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(io), service);
    tokio::pin!(conn);
    tokio::select! {
        result = conn.as_mut() => {
            if let Err(e) = result {
                tracing::debug!("connection ended: {e}");
            }
            return;
        }
        _ = stopped(&mut shutdown) => {}
    }
    conn.as_mut().graceful_shutdown();
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, conn).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_output() {
        let out = "p812\nccaddy\nf5\nf6\np900\ncnode\nf3\np812\nccaddy\n";
        assert_eq!(
            parse_lsof(out).as_deref(),
            Some("caddy (pid 812), node (pid 900)")
        );
        assert_eq!(parse_lsof(""), None);
    }

    #[test]
    fn default_config_listens_on_loopback() {
        let config = ProxyConfig::default();
        let https: Vec<String> = config.https_addrs.iter().map(|a| a.to_string()).collect();
        let http: Vec<String> = config.http_addrs.iter().map(|a| a.to_string()).collect();
        assert_eq!(https, ["127.0.0.1:443", "[::1]:443"]);
        assert_eq!(http, ["127.0.0.1:80", "[::1]:80"]);
        assert_eq!(config.ready_timeout, Duration::from_secs(120));
    }
}
