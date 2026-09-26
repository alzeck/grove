//! Forwarding a request to a process over HTTP/1.1, including upgrades.

use crate::body::{self, Body};
use crate::pages;
use crate::routes::{RequestHost, Route};
use http::header::{
    CONNECTION, EXPECT, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::convert::Infallible;
use std::future::{Ready, ready};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::watch;

pub(crate) type UpstreamClient = Client<HttpConnector<LoopbackResolver>, Incoming>;

/// Where a request came in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnInfo {
    pub peer: SocketAddr,
    pub local: SocketAddr,
}

pub(crate) fn upstream_client() -> UpstreamClient {
    let mut connector = HttpConnector::new_with_resolver(LoopbackResolver);
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    Client::builder(TokioExecutor::new())
        // Below Node's 5s keep-alive timeout, so we rarely reuse a connection
        // the server is about to close.
        .pool_idle_timeout(Duration::from_secs(4))
        .pool_timer(TokioTimer::new())
        .build(connector)
}

/// Resolves every upstream name to `127.0.0.1`, then `::1`. Dev servers that
/// listen on `localhost` often bind only `::1` (Node 17+), so a refused IPv4
/// connection falls back to IPv6.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LoopbackResolver;

impl tower_service::Service<Name> for LoopbackResolver {
    type Response = std::array::IntoIter<SocketAddr, 2>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Name) -> Self::Future {
        ready(Ok([
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
        ]
        .into_iter()))
    }
}

pub(crate) async fn forward(
    client: &UpstreamClient,
    mut req: Request<Incoming>,
    route: &Route,
    host: &RequestHost,
    conn: ConnInfo,
    shutdown: watch::Receiver<bool>,
) -> Response<Body> {
    let upgrade = upgrade_protocol(&req);
    let client_upgrade = upgrade.is_some().then(|| hyper::upgrade::on(&mut req));

    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    // The host is resolved by `LoopbackResolver`; it only names the pool.
    let Ok(uri) = Uri::try_from(format!("http://localhost:{}{path}", route.port)) else {
        return pages::bad_request("The request target isn't a path.");
    };
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    prepare_request_headers(&mut parts.headers, host, conn, upgrade);

    let mut response = match client.request(Request::from_parts(parts, body)).await {
        Ok(response) => response,
        Err(e) if e.is_connect() => return pages::upstream_down(&route.label, route.port),
        Err(e) => {
            tracing::debug!(label = %route.label, "upstream error: {e:?}");
            return pages::bad_gateway(&route.label, route.port, &error_chain(&e));
        }
    };

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade else {
            return pages::bad_gateway(
                &route.label,
                route.port,
                "It switched protocols without being asked to.",
            );
        };
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(tunnel(client_upgrade, upstream_upgrade, shutdown));
        let (mut parts, _) = response.into_parts();
        let protocol = parts.headers.get(UPGRADE).cloned();
        strip_hop_by_hop(&mut parts.headers);
        parts
            .headers
            .insert(CONNECTION, HeaderValue::from_static("upgrade"));
        if let Some(protocol) = protocol {
            parts.headers.insert(UPGRADE, protocol);
        }
        return Response::from_parts(parts, body::empty());
    }

    let (mut parts, body) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Response::from_parts(parts, body.boxed())
}

/// The `Upgrade` header of an HTTP/1.1 upgrade request (e.g. a websocket
/// handshake).
pub(crate) fn upgrade_protocol<B>(req: &Request<B>) -> Option<HeaderValue> {
    if req.version() != Version::HTTP_11 || !has_token(req.headers(), CONNECTION, "upgrade") {
        return None;
    }
    req.headers().get(UPGRADE).cloned()
}

fn prepare_request_headers(
    headers: &mut HeaderMap,
    host: &RequestHost,
    conn: ConnInfo,
    upgrade: Option<HeaderValue>,
) {
    strip_hop_by_hop(headers);
    // The client's `Expect: 100-continue` was already answered by our server.
    headers.remove(EXPECT);
    if let Some(protocol) = upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(UPGRADE, protocol);
    }
    headers.insert(HOST, host.raw.clone());
    append_forwarded_for(headers, conn.peer.ip());
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert("x-forwarded-host", host.raw.clone());
    headers.insert("x-forwarded-port", HeaderValue::from(conn.local.port()));
}

fn append_forwarded_for(headers: &mut HeaderMap, ip: IpAddr) {
    let mut chain: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let ip = ip.to_string();
    chain.push(&ip);
    let value = HeaderValue::try_from(chain.join(", ")).unwrap_or_else(|_| {
        HeaderValue::try_from(ip.as_str()).expect("an IP address is a valid header value")
    });
    headers.insert("x-forwarded-for", value);
}

/// Removes headers that describe a single connection (RFC 9110 §7.6.1),
/// including any listed in `Connection`.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in [CONNECTION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    let proxy: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("proxy-"))
        .cloned()
        .collect();
    for name in proxy
        .into_iter()
        .chain([PROXY_AUTHENTICATE, PROXY_AUTHORIZATION])
    {
        headers.remove(name);
    }
}

fn has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// Pipes bytes between the two upgraded connections until either side
/// closes or the proxy shuts down.
async fn tunnel(client: OnUpgrade, upstream: OnUpgrade, mut shutdown: watch::Receiver<bool>) {
    let pipe = async {
        let (client, upstream) = tokio::try_join!(client, upstream)?;
        let mut client = TokioIo::new(client);
        let mut upstream = TokioIo::new(upstream);
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    tokio::select! {
        result = pipe => {
            if let Err(e) = result {
                tracing::debug!("upgraded connection ended: {e}");
            }
        }
        _ = crate::server::stopped(&mut shutdown) => {}
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        message.push_str(": ");
        message.push_str(&e.to_string());
        source = e.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn strips_hop_by_hop_headers() {
        let mut map = headers(&[
            ("connection", "keep-alive, x-custom"),
            ("keep-alive", "timeout=5"),
            ("x-custom", "1"),
            ("proxy-connection", "keep-alive"),
            ("proxy-authorization", "secret"),
            ("te", "trailers"),
            ("trailer", "x-checksum"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("content-type", "text/plain"),
            ("cookie", "a=1"),
        ]);
        strip_hop_by_hop(&mut map);
        let mut left: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
        left.sort();
        assert_eq!(left, ["content-type", "cookie"]);
    }

    #[test]
    fn prepares_forwarded_headers() {
        let mut map = headers(&[
            ("x-forwarded-for", "10.0.0.1"),
            ("x-forwarded-proto", "http"),
            ("expect", "100-continue"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ]);
        let host = RequestHost {
            raw: HeaderValue::from_static("API.localhost:8443"),
            name: "api.localhost".into(),
        };
        let conn = ConnInfo {
            peer: "[::1]:5000".parse().unwrap(),
            local: "[::1]:8443".parse().unwrap(),
        };
        prepare_request_headers(
            &mut map,
            &host,
            conn,
            Some(HeaderValue::from_static("websocket")),
        );
        assert_eq!(map["host"], "API.localhost:8443");
        assert_eq!(map["x-forwarded-for"], "10.0.0.1, ::1");
        assert_eq!(map["x-forwarded-proto"], "https");
        assert_eq!(map["x-forwarded-host"], "API.localhost:8443");
        assert_eq!(map["x-forwarded-port"], "8443");
        assert_eq!(map["connection"], "upgrade");
        assert_eq!(map["upgrade"], "websocket");
        assert!(!map.contains_key("expect"));
    }

    #[test]
    fn detects_upgrade_requests() {
        let req = Request::builder()
            .header(CONNECTION, "keep-alive, Upgrade")
            .header(UPGRADE, "websocket")
            .body(())
            .unwrap();
        assert_eq!(upgrade_protocol(&req).unwrap(), "websocket");

        let req = Request::builder()
            .header(UPGRADE, "websocket")
            .body(())
            .unwrap();
        assert_eq!(upgrade_protocol(&req), None);

        let req = Request::builder()
            .version(Version::HTTP_2)
            .header(CONNECTION, "upgrade")
            .header(UPGRADE, "websocket")
            .body(())
            .unwrap();
        assert_eq!(upgrade_protocol(&req), None);
    }
}
