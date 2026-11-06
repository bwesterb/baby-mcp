//! A tiny page shell: shared CSS (light/dark) and script as versioned static
//! files, a strict CSP (only our own files; nothing inline), and HTML escaping.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// The app's name on the home screen: the baby's name, set once at startup.
pub static APP_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Tags that make the pages installable as a home-screen app.
fn app_head() -> String {
    let name = esc(APP_NAME.get().map(String::as_str).unwrap_or("baby-mcp"));
    format!(
        r##"<link rel="manifest" href="/manifest.webmanifest"><link rel="icon" href="/icon-192.png"><link rel="apple-touch-icon" href="/icon-180.png"><meta name="theme-color" content="#2f6f5e"><meta name="mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-title" content="{name}">"##
    )
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

pub const CSS: &str = include_str!("../static/app.css");

pub const JS: &str = include_str!("../static/app.js");

pub struct Page<'a> {
    pub title: &'a str,
    pub body: String,
    pub nav: Option<&'a str>, // logged-in display name
    pub refresh: Option<u32>,
}

pub fn render(p: Page) -> Response {
    let nav = match p.nav {
        Some(me) => format!(
            r#"<nav><a href="/" class="home" aria-label="Status"><img src="/elephant.png" width="22" height="22" alt=""><span class="lbl">Status</span></a><a href="/day" aria-label="Days">📅<span class="lbl"> Days</span></a><a href="/charts" aria-label="Charts">📈<span class="lbl"> Charts</span></a><a href="/add" aria-label="Add">✏️<span class="lbl"> Add</span></a><a href="/notifications" title="Notifications">🔔</a><span class="me">{}</span><form method="post" action="/logout"><button>Log out</button></form></nav>"#,
            esc(me)
        ),
        None => r#"<div class="brand logo"><img src="/icon-192.png" width="72" height="72" alt=""></div>"#.to_string(),
    };
    let refresh = p
        .refresh
        .map(|s| format!(r#"<meta http-equiv="refresh" content="{s}">"#))
        .unwrap_or_default();
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">{refresh}{head}<title>{title}</title><link rel="stylesheet" href="/app.css?v={v}"><script src="/app.js?v={v}" defer></script></head><body><main>{nav}{body}</main></body></html>"#,
        title = esc(p.title),
        head = app_head(),
        v = version(),
        body = p.body
    );
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    // Not no-referrer: then browsers send `Origin: null` on form posts, which the
    // same-origin check (rightly) refuses. same-origin still sends nothing to other sites.
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; manifest-src 'self'; worker-src 'self'; connect-src 'self'; form-action 'self' https:; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    (StatusCode::OK, h, html).into_response()
}

pub fn error_page(status: StatusCode, msg: &str) -> Response {
    let mut r = render(Page {
        title: "baby-mcp",
        body: format!(r#"<div class="card"><h1>Cannot continue</h1><p>{}</p></div>"#, esc(msg)),
        nav: None,
        refresh: None,
    });
    *r.status_mut() = status;
    r
}

/// Content version of the CSS and JS, for cache-busting URLs.
fn version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let d = Sha256::new().chain_update(CSS).chain_update(JS).finalize();
        d.iter().take(5).map(|b| format!("{b:02x}")).collect()
    })
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    // Versioned URLs: a change gives a new URL, so cache for a year.
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], body).into_response()
}

pub async fn css() -> Response {
    asset("text/css; charset=utf-8", CSS)
}

pub async fn js() -> Response {
    asset("text/javascript; charset=utf-8", JS)
}
