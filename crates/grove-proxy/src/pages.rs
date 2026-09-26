//! The proxy's own HTML pages. Self-contained: inline CSS, no external
//! assets, light and dark mode.

use crate::backend::IndexEntry;
use crate::body::{Body, full};
use http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderValue, Response, StatusCode};
use std::fmt::Write;
use std::time::Duration;

/// Set on the starting page so its poller can tell it apart from the app.
pub(crate) const PAGE_HEADER: &str = "x-grove-page";

const STYLE: &str = r#"
:root{color-scheme:light dark;--bg:#f6f6f3;--fg:#1c1c1a;--muted:#6c6c66;--line:#e2e2dc;--card:#fff;--accent:#2f7d4f;--warn:#c27c0e;--err:#c93a32;--off:#a3a39c}
@media (prefers-color-scheme:dark){:root{--bg:#131312;--fg:#ececea;--muted:#9b9b95;--line:#2b2b28;--card:#1b1b19;--accent:#6fcf97;--warn:#e8a93a;--err:#f07068;--off:#6c6c66}}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:48px 24px;background:var(--bg);color:var(--fg);font:15px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif;-webkit-font-smoothing:antialiased}
main{width:100%;max-width:560px}
.brand{display:flex;align-items:center;gap:8px;margin:0 0 24px;font-size:12px;font-weight:600;letter-spacing:.08em;text-transform:uppercase;color:var(--muted)}
.brand::before{content:"";width:10px;height:10px;border-radius:50% 0;background:var(--accent)}
h1{margin:0 0 6px;font-size:22px;line-height:1.3;font-weight:600;letter-spacing:-.01em;overflow-wrap:anywhere}
p{margin:0;color:var(--muted)}
code{font:.9em ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--fg);overflow-wrap:anywhere}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline;text-underline-offset:2px}
.clusters{display:grid;gap:10px;margin-top:28px}
.cluster{padding:14px 16px;background:var(--card);border:1px solid var(--line);border-radius:10px}
.cluster h2{display:flex;align-items:center;gap:8px;margin:0;font-size:15px;font-weight:600}
.state{margin-left:auto;font-size:12px;font-weight:500;color:var(--muted)}
.dot{flex:none;width:8px;height:8px;border-radius:50%;background:var(--off)}
.dot.running,.dot.ready{background:var(--accent)}
.dot.starting{background:var(--warn)}
.dot.error,.dot.crashed{background:var(--err)}
.cluster ul{display:grid;gap:4px;margin:10px 0 0;padding:0;list-style:none}
.cluster li{display:flex;justify-content:space-between;gap:16px;font-size:14px}
.cluster li a{overflow-wrap:anywhere}
.cluster li span{flex:none;color:var(--muted)}
.cluster .none{margin-top:6px;font-size:14px}
.empty{margin-top:28px}
.spinner{width:20px;height:20px;margin-bottom:22px;border:2px solid var(--line);border-top-color:var(--accent);border-radius:50%;animation:spin .8s linear infinite}
@keyframes spin{to{transform:rotate(360deg)}}
@media (prefers-reduced-motion:reduce){.spinner{animation-duration:3s}}
"#;

/// Polls the page while the cluster starts and reloads once the proxy
/// serves anything other than this page.
const STARTING_HEAD: &str = r#"<noscript><meta http-equiv="refresh" content="1"></noscript>
<script>
(function poll() {
  setTimeout(function () {
    fetch(location.href, { headers: { Accept: "text/html" }, cache: "no-store" })
      .then(function (r) { if (r.headers.get("x-grove-page") === "starting") poll(); else location.reload(); })
      .catch(poll);
  }, 1000);
})();
</script>"#;

pub(crate) fn unknown_host(host: Option<&str>, index: &[IndexEntry]) -> Response<Body> {
    let mut body = String::from("<h1>Unknown host</h1>");
    match host {
        Some(host) => write!(
            body,
            "<p><code>{}</code> doesn't match any process in Grove.</p>",
            esc(host)
        )
        .unwrap(),
        None => body.push_str("<p>The request didn't say which host it was for.</p>"),
    }
    if index.is_empty() {
        body.push_str(r#"<p class="empty">There are no clusters yet.</p>"#);
    } else {
        body.push_str(r#"<section class="clusters">"#);
        for entry in index {
            write_cluster(&mut body, entry);
        }
        body.push_str("</section>");
    }
    page(StatusCode::NOT_FOUND, "Unknown host", "", &body)
}

pub(crate) fn starting(cluster: &str) -> Response<Body> {
    let body = format!(
        r#"<div class="spinner" aria-hidden="true"></div><h1>Starting {}…</h1><p>This page reloads when it's ready.</p>"#,
        esc(cluster)
    );
    let mut response = page(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("Starting {cluster}"),
        STARTING_HEAD,
        &body,
    );
    let headers = response.headers_mut();
    headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
    headers.insert(PAGE_HEADER, HeaderValue::from_static("starting"));
    response
}

pub(crate) fn stopped(cluster: &str) -> Response<Body> {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("Cluster {cluster} is stopped"),
        "Start it from Grove.",
    )
}

pub(crate) fn idle(cluster: &str) -> Response<Body> {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("Cluster {cluster} is idle"),
        "Load a page from it to wake it up.",
    )
}

pub(crate) fn not_ready(cluster: &str, waited: Duration) -> Response<Body> {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("Cluster {cluster} didn't become ready"),
        &format!(
            "Gave up after {}s. Check its processes in Grove.",
            waited.as_secs()
        ),
    )
}

pub(crate) fn upstream_down(label: &str, port: u16) -> Response<Body> {
    message(
        StatusCode::BAD_GATEWAY,
        &format!("{label} isn't responding on port {port}"),
        "It may still be starting, or it may have crashed. Check its output in Grove.",
    )
}

pub(crate) fn bad_gateway(label: &str, port: u16, error: &str) -> Response<Body> {
    message(
        StatusCode::BAD_GATEWAY,
        &format!("{label} on port {port} failed to respond"),
        error,
    )
}

pub(crate) fn bad_request(reason: &str) -> Response<Body> {
    message(StatusCode::BAD_REQUEST, "Bad request", reason)
}

fn message(status: StatusCode, title: &str, text: &str) -> Response<Body> {
    let body = format!("<h1>{}</h1><p>{}</p>", esc(title), esc(text));
    page(status, title, "", &body)
}

fn write_cluster(out: &mut String, entry: &IndexEntry) {
    write!(
        out,
        r#"<article class="cluster"><h2><span class="dot {}"></span>{}<span class="state">{}</span></h2>"#,
        state_class(&entry.state),
        esc(&entry.cluster),
        esc(&entry.state)
    )
    .unwrap();
    if entry.urls.is_empty() {
        out.push_str(r#"<p class="none">No domains.</p>"#);
    } else {
        out.push_str("<ul>");
        for (label, url) in &entry.urls {
            let shown = url.strip_prefix("https://").unwrap_or(url);
            write!(
                out,
                r#"<li><a href="{}">{}</a><span>{}</span></li>"#,
                esc(url),
                esc(shown),
                esc(label)
            )
            .unwrap();
        }
        out.push_str("</ul>");
    }
    out.push_str("</article>");
}

/// `"crashed(1)"` → `"crashed"`, safe to use as a class name.
fn state_class(state: &str) -> String {
    state
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn page(status: StatusCode, title: &str, head: &str, body: &str) -> Response<Body> {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{} · Grove</title>
<style>{STYLE}</style>
{head}
</head>
<body><main><p class="brand">Grove</p>{body}</main></body>
</html>
"#,
        esc(title)
    );
    let mut response = Response::new(full(html));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn text(response: Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn index_lists_clusters_and_escapes() {
        let index = vec![
            IndexEntry {
                cluster: "default".into(),
                state: "running".into(),
                urls: vec![("api.web".into(), "https://api.localhost".into())],
            },
            IndexEntry {
                cluster: "pr-1".into(),
                state: "crashed(1)".into(),
                urls: vec![],
            },
        ];
        let response = unknown_host(Some("<x>.localhost"), &index);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let html = text(response).await;
        assert!(html.contains("<code>&lt;x&gt;.localhost</code>"));
        assert!(html.contains(r#"<a href="https://api.localhost">api.localhost</a>"#));
        assert!(html.contains(r#"<span class="dot crashed"></span>pr-1"#));
        assert!(!html.contains("<x>"));
    }

    #[tokio::test]
    async fn starting_page_polls() {
        let response = starting("pr-1");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        assert_eq!(response.headers()[PAGE_HEADER], "starting");
        let html = text(response).await;
        assert!(html.contains("Starting pr-1…"));
        assert!(html.contains("x-grove-page"));
    }
}
