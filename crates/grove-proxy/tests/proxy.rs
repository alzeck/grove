//! End-to-end: a real upstream, the proxy on port 0, and HTTPS clients that
//! trust only the test CA.

use async_trait::async_trait;
use bytes::Bytes;
use grove_proxy::{
    Availability, Backend, CertAuthority, IndexEntry, Proxy, ProxyConfig, ProxyError, Route,
};
use http::header::{
    ACCEPT, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, LOCATION, RETRY_AFTER, UPGRADE,
};
use http::{Method, Request, Response, StatusCode, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

async fn within<F: Future>(test: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, test)
        .await
        .expect("test timed out")
}

// ---------------------------------------------------------------------------
// Fake backend

#[derive(Default)]
struct FakeBackend {
    states: Arc<Mutex<HashMap<String, Availability>>>,
    activity: Mutex<HashMap<String, usize>>,
    wakes: Mutex<Vec<String>>,
}

impl FakeBackend {
    fn set(&self, cluster: &str, availability: Availability) {
        self.states.lock().insert(cluster.to_owned(), availability);
    }

    fn activity(&self, cluster: &str) -> usize {
        self.activity.lock().get(cluster).copied().unwrap_or(0)
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn availability(&self, cluster: &str) -> Availability {
        self.states
            .lock()
            .get(cluster)
            .copied()
            .unwrap_or(Availability::Unknown)
    }

    fn record_activity(&self, cluster: &str) {
        *self.activity.lock().entry(cluster.to_owned()).or_default() += 1;
    }

    /// Becomes ready a little later, like a real cluster.
    fn wake(&self, cluster: &str) {
        self.wakes.lock().push(cluster.to_owned());
        self.set(cluster, Availability::Starting);
        let states = self.states.clone();
        let cluster = cluster.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            states.lock().insert(cluster, Availability::Ready);
        });
    }

    async fn wait_until_ready(&self, cluster: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.availability(cluster) == Availability::Ready {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn index(&self) -> Vec<IndexEntry> {
        vec![IndexEntry {
            cluster: "default".into(),
            state: "running".into(),
            urls: vec![("api.web".into(), "https://api.localhost".into())],
        }]
    }
}

// ---------------------------------------------------------------------------
// Upstream

type UpstreamBody = BoxBody<Bytes, Infallible>;

async fn start_upstream() -> (SocketAddr, Arc<Notify>) {
    start_upstream_on("127.0.0.1:0").await
}

async fn start_upstream_on(bind: &str) -> (SocketAddr, Arc<Notify>) {
    let listener = TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let release = Arc::new(Notify::new());
    let release_for_server = release.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let release = release_for_server.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| upstream(req, release.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    (addr, release)
}

async fn upstream(
    mut req: Request<Incoming>,
    release: Arc<Notify>,
) -> Result<Response<UpstreamBody>, Infallible> {
    let full = |text: String| Full::new(Bytes::from(text)).boxed();
    let response = match req.uri().path() {
        "/headers" => {
            let mut lines: Vec<String> = req
                .headers()
                .iter()
                .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap()))
                .collect();
            lines.push(format!("version: {:?}", req.version()));
            lines.push(format!("uri: {}", req.uri()));
            Response::new(full(lines.join("\n")))
        }
        "/body" => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::new(full(format!("got {}", String::from_utf8_lossy(&body))))
        }
        // One chunk now, the next only once the test says so.
        "/stream" => {
            let (tx, rx) = mpsc::channel::<Bytes>(1);
            tokio::spawn(async move {
                let _ = tx.send(Bytes::from("first")).await;
                release.notified().await;
                let _ = tx.send(Bytes::from("second")).await;
            });
            let frames = futures::stream::unfold(rx, |mut rx| async move {
                let chunk = rx.recv().await?;
                Some((Ok::<_, Infallible>(Frame::data(chunk)), rx))
            });
            Response::new(StreamBody::new(frames).boxed())
        }
        "/echo" => {
            let on_upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                if let Ok(upgraded) = on_upgrade.await {
                    let (mut reader, mut writer) = tokio::io::split(TokioIo::new(upgraded));
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                }
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(CONNECTION, "upgrade")
                .header(UPGRADE, "echo")
                .body(Empty::new().boxed())
                .unwrap()
        }
        _ => Response::new(full("hello from upstream".into())),
    };
    Ok(response)
}

// ---------------------------------------------------------------------------
// Proxy under test

struct Env {
    _tmp: tempfile::TempDir,
    ca: Arc<CertAuthority>,
    backend: Arc<FakeBackend>,
    proxy: Proxy,
    https: SocketAddr,
    http: SocketAddr,
    release: Arc<Notify>,
    down_port: u16,
}

fn route(host: &str, wildcard: bool, cluster: &str, label: &str, port: u16) -> Route {
    Route {
        host: host.into(),
        wildcard,
        cluster: cluster.into(),
        label: label.into(),
        port,
    }
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

async fn env() -> Env {
    env_with(Duration::from_secs(5)).await
}

async fn env_with(ready_timeout: Duration) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let ca = Arc::new(CertAuthority::load_or_create(&tmp.path().join("ca")).unwrap());
    let backend = Arc::new(FakeBackend::default());
    backend.set("default", Availability::Ready);
    backend.set("old", Availability::Stopped);
    backend.set("sleepy", Availability::Idle);
    backend.set("booting", Availability::Starting);

    let (upstream, release) = start_upstream().await;
    let config = ProxyConfig {
        https_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        http_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ready_timeout,
    };
    let proxy = Proxy::start(config, ca.clone(), backend.clone())
        .await
        .unwrap();
    let port = upstream.port();
    let down_port = free_port().await;
    proxy.set_routes(vec![
        route("api.localhost", true, "default", "api.web", port),
        route("old.localhost", false, "old", "old.web", port),
        route("sleepy.localhost", false, "sleepy", "sleepy.web", port),
        route("booting.localhost", false, "booting", "booting.web", port),
        route("down.localhost", false, "default", "api.worker", down_port),
    ]);
    let (https, http) = proxy.local_addrs();
    Env {
        _tmp: tmp,
        ca,
        backend,
        proxy,
        https: https[0],
        http: http[0],
        release,
        down_port,
    }
}

impl Env {
    async fn tls(&self, sni: &str, alpn: &[&[u8]]) -> TlsStream<TcpStream> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(self.ca.cert_pem().as_bytes()).unwrap())
            .unwrap();
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let tcp = TcpStream::connect(self.https).await.unwrap();
        TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from(sni.to_owned()).unwrap(), tcp)
            .await
            .unwrap()
    }

    /// Sends one HTTP/1.1 request over TLS, using the `Host` header as SNI.
    async fn send<B>(&self, req: Request<B>) -> Response<Incoming>
    where
        B: hyper::body::Body + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let host = req.headers()[HOST].to_str().unwrap().to_owned();
        let tls = self.tls(&host, &[b"http/1.1"]).await;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(conn.with_upgrades());
        sender.send_request(req).await.unwrap()
    }
}

fn get(host: &str, path: &str) -> Request<Empty<Bytes>> {
    Request::get(path)
        .header(HOST, host)
        .body(Empty::new())
        .unwrap()
}

async fn text(response: Response<Incoming>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn next_chunk(body: &mut Incoming) -> Bytes {
    loop {
        let frame = body.frame().await.expect("body ended").unwrap();
        if let Ok(data) = frame.into_data() {
            return data;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxies_with_forwarded_headers() {
    within(async {
        let env = env().await;
        let response = env.send(get("api.localhost", "/headers?x=1")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = text(response).await;
        let lines: Vec<&str> = body.lines().collect();
        for expected in [
            "host: api.localhost",
            "x-forwarded-proto: https",
            "x-forwarded-host: api.localhost",
            "x-forwarded-for: 127.0.0.1",
            &format!("x-forwarded-port: {}", env.https.port()),
            "version: HTTP/1.1",
            "uri: /headers?x=1",
        ] {
            assert!(lines.contains(&expected), "missing {expected:?} in\n{body}");
        }
        assert_eq!(env.backend.activity("default"), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_keeps_content_length() {
    within(async {
        let env = env().await;
        let mut req = get("api.localhost", "/");
        *req.method_mut() = Method::HEAD;
        let response = env.send(req).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_LENGTH], "19");
        assert_eq!(text(response).await, "");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_ipv6_loopback() {
    within(async {
        let env = env().await;
        let (upstream, _) = start_upstream_on("[::1]:0").await;
        env.proxy.set_routes(vec![route(
            "v6.localhost",
            false,
            "default",
            "v6.web",
            upstream.port(),
        )]);
        let response = env.send(get("v6.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, "hello from upstream");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_request_bodies() {
    within(async {
        let env = env().await;
        let req = Request::post("/body")
            .header(HOST, "api.localhost")
            .body(Full::new(Bytes::from("payload")))
            .unwrap();
        let response = env.send(req).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, "got payload");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_routes_get_their_own_certificate() {
    within(async {
        let env = env().await;
        let response = env.send(get("tenant.api.localhost", "/headers")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = text(response).await;
        assert!(
            body.lines().any(|l| l == "host: tenant.api.localhost"),
            "{body}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_host_lists_clusters() {
    within(async {
        let env = env().await;
        let response = env.send(get("nope.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        let html = text(response).await;
        assert!(html.contains("<code>nope.localhost</code>"), "{html}");
        assert!(html.contains(r#"href="https://api.localhost""#), "{html}");
        assert_eq!(env.backend.activity("default"), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_cluster_gets_503() {
    within(async {
        let env = env().await;
        let response = env.send(get("old.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let html = text(response).await;
        assert!(html.contains("Cluster old is stopped"), "{html}");
        assert!(html.contains("Start it from Grove."), "{html}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_cluster_wakes_and_holds_the_request() {
    within(async {
        let env = env().await;
        let started = Instant::now();
        let response = env.send(get("sleepy.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(text(response).await, "hello from upstream");
        assert_eq!(*env.backend.wakes.lock(), ["sleepy"]);
        assert_eq!(env.backend.activity("sleepy"), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigation_while_starting_gets_the_starting_page() {
    within(async {
        let env = env().await;
        let mut req = get("booting.localhost", "/");
        req.headers_mut().insert(
            ACCEPT,
            "text/html,application/xhtml+xml,*/*;q=0.8".parse().unwrap(),
        );
        let response = env.send(req).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let html = text(response).await;
        assert!(html.contains("Starting booting…"), "{html}");
        assert_eq!(env.backend.activity("booting"), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_request_gives_up_after_ready_timeout() {
    within(async {
        let env = env_with(Duration::from_millis(200)).await;
        let response = env.send(get("booting.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let html = text(response).await;
        assert!(
            html.contains("Cluster booting didn't become ready"),
            "{html}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_down_gets_502() {
    within(async {
        let env = env().await;
        let response = env.send(get("down.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let html = text(response).await;
        let expected = format!("api.worker isn't responding on port {}", env.down_port);
        assert!(html.contains(&expected), "{html}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_round_trip() {
    within(async {
        let env = env().await;
        let req = Request::get("/echo")
            .header(HOST, "api.localhost")
            .header(CONNECTION, "Upgrade")
            .header(UPGRADE, "echo")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = env.send(req).await;
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.headers()[UPGRADE], "echo");
        let mut io = TokioIo::new(hyper::upgrade::on(response).await.unwrap());
        for message in [&b"ping"[..], b"second message"] {
            io.write_all(message).await.unwrap();
            let mut echoed = vec![0; message.len()];
            io.read_exact(&mut echoed).await.unwrap();
            assert_eq!(echoed, message);
        }
        // Websocket traffic isn't activity.
        assert_eq!(env.backend.activity("default"), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_responses_incrementally() {
    within(async {
        let env = env().await;
        let response = env.send(get("api.localhost", "/stream")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        // The upstream only sends "second" after we've seen "first", so a
        // buffering proxy would hang here.
        assert_eq!(next_chunk(&mut body).await, "first");
        env.release.notify_one();
        assert_eq!(next_chunk(&mut body).await, "second");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_clients_are_proxied_over_http1() {
    within(async {
        let env = env().await;
        let tls = env.tls("api.localhost", &[b"h2"]).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
                .await
                .unwrap();
        tokio::spawn(conn);
        let req = Request::get("https://api.localhost/headers")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = sender.send_request(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), Version::HTTP_2);
        let body = text(response).await;
        let lines: Vec<&str> = body.lines().collect();
        assert!(lines.contains(&"host: api.localhost"), "{body}");
        assert!(lines.contains(&"version: HTTP/1.1"), "{body}");
        assert_eq!(env.backend.activity("default"), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_listener_redirects_to_https() {
    within(async {
        let env = env().await;
        let tcp = TcpStream::connect(env.http).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = get("api.localhost", "/a/b?c=1");
        *req.method_mut() = Method::POST;
        let response = sender.send_request(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[LOCATION],
            format!("https://api.localhost:{}/a/b?c=1", env.https.port()).as_str()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_can_be_swapped() {
    within(async {
        let env = env().await;
        env.proxy.set_routes(vec![route(
            "fresh.localhost",
            false,
            "default",
            "fresh.web",
            env.down_port,
        )]);
        let response = env.send(get("api.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = env.send(get("fresh.localhost", "/")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_listeners_and_tunnels() {
    within(async {
        let env = env().await;
        let req = Request::get("/echo")
            .header(HOST, "api.localhost")
            .header(CONNECTION, "upgrade")
            .header(UPGRADE, "echo")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = env.send(req).await;
        let mut io = TokioIo::new(hyper::upgrade::on(response).await.unwrap());
        io.write_all(b"hi").await.unwrap();
        let mut buf = [0; 2];
        io.read_exact(&mut buf).await.unwrap();

        let https = env.https;
        env.proxy.shutdown().await;
        assert!(TcpStream::connect(https).await.is_err());
        let mut rest = Vec::new();
        let _ = io.read_to_end(&mut rest).await;
        assert!(rest.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_conflict_names_the_owner() {
    within(async {
        let tmp = tempfile::tempdir().unwrap();
        let ca = Arc::new(CertAuthority::load_or_create(tmp.path()).unwrap());
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();
        let config = ProxyConfig {
            https_addrs: vec![addr],
            http_addrs: vec![],
            ready_timeout: Duration::from_secs(1),
        };
        match Proxy::start(config, ca, Arc::new(FakeBackend::default())).await {
            Err(ProxyError::Bind {
                addr: failed,
                source,
                owner,
            }) => {
                assert_eq!(failed, addr);
                assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
                let owner = owner.expect("lsof finds our own listener");
                assert!(
                    owner.contains(&format!("(pid {})", std::process::id())),
                    "{owner}"
                );
            }
            other => panic!("expected a bind error, got {other:?}"),
        }
    })
    .await;
}
