use http::header::HOST;
use http::uri::Authority;
use http::{HeaderValue, Request};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Lowercase host name, e.g. `api-pr-1.localhost`.
    pub host: String,
    /// Also match any subdomain (`*.host`).
    pub wildcard: bool,
    pub cluster: String,
    /// `project.process`, shown on error pages.
    pub label: String,
    pub port: u16,
}

/// Routes indexed for lookup: exact host first, then the longest wildcard
/// suffix.
#[derive(Debug, Default)]
pub(crate) struct RouteTable {
    exact: HashMap<String, Route>,
    wildcard: HashMap<String, Route>,
}

impl RouteTable {
    pub(crate) fn new(routes: Vec<Route>) -> Self {
        let mut table = Self::default();
        for mut route in routes {
            route.host = normalize(&route.host);
            if table.exact.contains_key(&route.host) {
                tracing::warn!(host = %route.host, label = %route.label, "duplicate route ignored");
                continue;
            }
            if route.wildcard {
                table.wildcard.insert(route.host.clone(), route.clone());
            }
            table.exact.insert(route.host.clone(), route);
        }
        table
    }

    /// `host` must already be normalised (see [`RequestHost::name`]).
    pub(crate) fn lookup(&self, host: &str) -> Option<&Route> {
        if let Some(route) = self.exact.get(host) {
            return Some(route);
        }
        let mut rest = host;
        while let Some((_, parent)) = rest.split_once('.') {
            if let Some(route) = self.wildcard.get(parent) {
                return Some(route);
            }
            rest = parent;
        }
        None
    }
}

/// The host a request is addressed to.
#[derive(Debug, Clone)]
pub(crate) struct RequestHost {
    /// As the client sent it (`Host` header or HTTP/2 `:authority`), port
    /// included. Forwarded upstream unchanged.
    pub raw: HeaderValue,
    /// Lowercase, without port or trailing dot. Used for routing.
    pub name: String,
}

/// The URI authority wins over `Host`: it is HTTP/2's `:authority`, or the
/// target of an absolute-form HTTP/1.1 request (RFC 9112 §3.2.2).
pub(crate) fn request_host<B>(req: &Request<B>) -> Option<RequestHost> {
    let (raw, authority) = match req.uri().authority() {
        Some(authority) => (
            HeaderValue::from_str(authority.as_str()).ok()?,
            authority.clone(),
        ),
        None => {
            let raw = req.headers().get(HOST)?.clone();
            let authority = raw.to_str().ok()?.parse::<Authority>().ok()?;
            (raw, authority)
        }
    };
    let name = normalize(authority.host());
    (!name.is_empty()).then_some(RequestHost { raw, name })
}

fn normalize(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: &str, wildcard: bool, label: &str) -> Route {
        Route {
            host: host.into(),
            wildcard,
            cluster: "default".into(),
            label: label.into(),
            port: 3000,
        }
    }

    fn label<'a>(table: &'a RouteTable, host: &str) -> Option<&'a str> {
        table.lookup(host).map(|r| r.label.as_str())
    }

    #[test]
    fn exact_then_longest_wildcard() {
        let table = RouteTable::new(vec![
            route("localhost", true, "catch-all"),
            route("api.localhost", true, "api"),
            route("admin.api.localhost", false, "admin"),
            route("web.localhost", false, "web"),
        ]);
        assert_eq!(label(&table, "api.localhost"), Some("api"));
        assert_eq!(label(&table, "tenant.api.localhost"), Some("api"));
        assert_eq!(label(&table, "a.b.api.localhost"), Some("api"));
        assert_eq!(label(&table, "admin.api.localhost"), Some("admin"));
        assert_eq!(label(&table, "x.admin.api.localhost"), Some("api"));
        assert_eq!(label(&table, "web.localhost"), Some("web"));
        assert_eq!(label(&table, "x.web.localhost"), Some("catch-all"));
        assert_eq!(label(&table, "example.com"), None);
    }

    #[test]
    fn non_wildcard_matches_only_exact() {
        let table = RouteTable::new(vec![route("web.localhost", false, "web")]);
        assert_eq!(label(&table, "x.web.localhost"), None);
    }

    #[test]
    fn first_duplicate_wins_and_hosts_are_normalised() {
        let table = RouteTable::new(vec![
            route("API.localhost.", true, "first"),
            route("api.localhost", true, "second"),
        ]);
        assert_eq!(label(&table, "api.localhost"), Some("first"));
        assert_eq!(label(&table, "x.api.localhost"), Some("first"));
    }

    fn host_of(req: Request<()>) -> Option<(String, String)> {
        request_host(&req).map(|h| (h.raw.to_str().unwrap().to_owned(), h.name))
    }

    #[test]
    fn host_from_header_or_authority() {
        let req = Request::builder()
            .uri("/path")
            .header(HOST, "API.Localhost:8443")
            .body(())
            .unwrap();
        assert_eq!(
            host_of(req),
            Some(("API.Localhost:8443".into(), "api.localhost".into()))
        );

        let req = Request::builder()
            .uri("https://web.localhost/path")
            .header(HOST, "ignored.localhost")
            .body(())
            .unwrap();
        assert_eq!(
            host_of(req),
            Some(("web.localhost".into(), "web.localhost".into()))
        );

        let req = Request::builder()
            .uri("/")
            .header(HOST, "[::1]:443")
            .body(())
            .unwrap();
        assert_eq!(host_of(req).unwrap().1, "[::1]");

        let req = Request::builder().uri("/").body(()).unwrap();
        assert_eq!(host_of(req), None);

        let req = Request::builder()
            .uri("/")
            .header(HOST, "bad host")
            .body(())
            .unwrap();
        assert_eq!(host_of(req), None);
    }
}
