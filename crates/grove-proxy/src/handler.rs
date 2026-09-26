//! Per-request decisions: which route, whether the cluster can take the
//! request, and the HTTP → HTTPS redirect.

use crate::backend::{Availability, Backend};
use crate::body::{self, Body};
use crate::forward::{self, ConnInfo, UpstreamClient};
use crate::pages;
use crate::routes::{RouteTable, request_host};
use http::header::{ACCEPT, CACHE_CONTROL, LOCATION};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub(crate) struct Shared {
    pub routes: RwLock<Arc<RouteTable>>,
    pub backend: Arc<dyn Backend>,
    pub client: UpstreamClient,
    pub ready_timeout: Duration,
    pub shutdown: watch::Receiver<bool>,
}

pub(crate) async fn handle(
    shared: Arc<Shared>,
    conn: ConnInfo,
    req: Request<Incoming>,
) -> Response<Body> {
    let Some(host) = request_host(&req) else {
        return pages::unknown_host(None, &shared.backend.index());
    };
    let routes = shared.routes.read().clone();
    let Some(route) = routes.lookup(&host.name) else {
        return pages::unknown_host(Some(&host.name), &shared.backend.index());
    };
    let backend = &shared.backend;
    let cluster = route.cluster.as_str();
    // Websockets don't count as activity, so a forgotten tab's hot-reload
    // socket neither wakes an idle cluster nor keeps it awake.
    let upgrade = forward::upgrade_protocol(&req).is_some();

    let starting = match backend.availability(cluster) {
        Availability::Ready => false,
        Availability::Starting => true,
        Availability::Idle if upgrade => return pages::idle(cluster),
        Availability::Idle => {
            backend.wake(cluster);
            true
        }
        Availability::Stopped | Availability::Unknown => return pages::stopped(cluster),
    };
    if starting {
        if is_navigation(&req) {
            return pages::starting(cluster);
        }
        if !backend
            .wait_until_ready(cluster, shared.ready_timeout)
            .await
        {
            return pages::not_ready(cluster, shared.ready_timeout);
        }
    }

    if !upgrade {
        backend.record_activity(cluster);
    }
    forward::forward(
        &shared.client,
        req,
        route,
        &host,
        conn,
        shared.shutdown.clone(),
    )
    .await
}

/// A browser loading a page, as opposed to fetch/XHR or an API client.
fn is_navigation<B>(req: &Request<B>) -> bool {
    req.method() == Method::GET
        && req
            .headers()
            .get_all(ACCEPT)
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.contains("text/html")))
}

/// `http://host/path` → `https://host[:https_port]/path` with 308.
pub(crate) fn redirect_to_https<B>(req: &Request<B>, https_port: u16) -> Response<Body> {
    let Some(host) = request_host(req) else {
        return pages::bad_request("The request has no Host header.");
    };
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let location = match https_port {
        443 => format!("https://{}{path}", host.name),
        port => format!("https://{}:{port}{path}", host.name),
    };
    let Ok(location) = HeaderValue::try_from(location) else {
        return pages::bad_request("The request target isn't a path.");
    };
    let mut response = Response::new(body::empty());
    *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
    response.headers_mut().insert(LOCATION, location);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HOST;

    #[test]
    fn navigation_needs_get_and_html() {
        let req = |method: Method, accept: &str| {
            Request::builder()
                .method(method)
                .header(ACCEPT, accept)
                .body(())
                .unwrap()
        };
        assert!(is_navigation(&req(
            Method::GET,
            "text/html,application/xhtml+xml,*/*;q=0.8"
        )));
        assert!(!is_navigation(&req(Method::GET, "*/*")));
        assert!(!is_navigation(&req(Method::POST, "text/html")));
    }

    #[test]
    fn redirect_keeps_path_and_query() {
        let req = Request::builder()
            .uri("/a/b?c=1")
            .header(HOST, "Api.localhost:80")
            .body(())
            .unwrap();
        let response = redirect_to_https(&req, 443);
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[LOCATION],
            "https://api.localhost/a/b?c=1"
        );
        assert_eq!(
            redirect_to_https(&req, 8443).headers()[LOCATION],
            "https://api.localhost:8443/a/b?c=1"
        );
    }
}
