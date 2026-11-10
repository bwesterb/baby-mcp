//! The notifications page, the push endpoints, and the service worker.

use crate::app::App;
use crate::auth::{User, same_origin, session_user, to_login};
use crate::html::{self, Page, esc};
use crate::notify::{self, KINDS, Notice};
use crate::time::now;
use axum::Json;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// A parent (a viewer gets no notifications: to the charts).
fn require(app: &App, headers: &HeaderMap) -> Result<User, Response> {
    let user = session_user(app, headers).ok_or_else(|| to_login("/notifications"))?;
    if user.viewer {
        return Err(Redirect::to("/charts").into_response());
    }
    Ok(user)
}

/// A parent, for the JSON endpoints.
fn parent(app: &App, headers: &HeaderMap) -> Option<User> {
    session_user(app, headers).filter(|u| !u.viewer)
}


/// A short name for a device from its User-Agent.
fn device_label(headers: &HeaderMap) -> String {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let os = [("iPhone", "iPhone"), ("iPad", "iPad"), ("Android", "Android"), ("Macintosh", "Mac"), ("Windows", "Windows"), ("Linux", "Linux")]
        .iter()
        .find(|(k, _)| ua.contains(k))
        .map(|(_, v)| *v)
        .unwrap_or("Device");
    let browser = if ua.contains("Firefox/") {
        " · Firefox"
    } else if ua.contains("Edg/") {
        " · Edge"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        " · Chrome"
    } else if ua.contains("Safari/") {
        " · Safari"
    } else {
        ""
    };
    format!("{os}{browser}")
}

/// A device: id, label, added, last delivered, last error.
type Device = (i64, String, i64, Option<i64>, Option<String>);

pub async fn page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let user = match require(&app, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let data = app.db.with(|c| {
        let vapid = crate::push::vapid(c)?;
        let prefs: Vec<bool> = KINDS.iter().map(|(k, _, _)| notify::wants(c, user.id, k)).collect::<rusqlite::Result<_>>()?;
        let mut st = c.prepare("SELECT id, label, created_at, last_ok, last_error FROM push_subscription WHERE user_id = ?1 ORDER BY created_at")?;
        let devices: Vec<Device> =
            st.query_map([user.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok((vapid.public, prefs, devices))
    });
    let (key, prefs, devices) = match data {
        Ok(d) => d,
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut body = format!(
        r#"<div class="card" id="push" data-key="{}"><h1>🔔 Notifications</h1><div class="head"><p id="dev" class="mut">Checking…</p><div class="btns"><button id="on" class="primary" hidden>Turn on</button><button id="off" hidden>Turn off</button><button id="test" hidden>Send test</button></div></div><p id="status"></p></div>"#,
        esc(&key)
    );
    body += r#"<div class="card"><h2>What to send me</h2>"#;
    // In sections: feeding, entries, care.
    const SECTIONS: [(&str, &[&str]); 3] = [
        ("🍼 Feeding", &["feed_soon", "feed_overdue", "feed_warning"]),
        ("📝 Entries", &["attention", "changes"]),
        ("💊 Care", &["vitamins"]),
    ];
    let listed: Vec<&str> = SECTIONS.iter().flat_map(|(_, ks)| ks.iter().copied()).collect();
    let rest: Vec<&str> = KINDS.iter().map(|k| k.0).filter(|k| !listed.contains(k)).collect();
    for (heading, kinds) in SECTIONS.iter().map(|(h, ks)| (*h, ks.to_vec())).chain((!rest.is_empty()).then(|| ("Other", rest.clone()))) {
        body += &format!(r#"<h3 class="section">{}</h3>"#, esc(heading));
        for kind in kinds {
            let Some(i) = KINDS.iter().position(|k| k.0 == kind) else { continue };
            let (_, label, when) = KINDS[i];
            body += &format!(
                r#"<p><label class="check"><input type="checkbox" data-kind="{kind}"{}><span><b>{}</b> <span class="mut">— {}</span></span></label></p>"#,
                if prefs[i] { " checked" } else { "" },
                esc(label),
                esc(when)
            );
        }
    }
    body += r#"<p class="mut">For all your devices; the other parent chooses their own. <span id="saved"></span></p></div>"#;
    body += r#"<div class="card"><h2>Your devices</h2>"#;
    if devices.is_empty() {
        body += r#"<p class="mut">None yet.</p>"#;
    } else {
        body += "<table>";
        for (id, label, created, last_ok, last_error) in devices {
            let state = match (last_ok, last_error) {
                (_, Some(e)) => format!("last send failed: {}", esc(&e)),
                (Some(t), None) => format!("last delivered {}", esc(&app.clock.fmt(t))),
                (None, None) => "nothing sent yet".into(),
            };
            body += &format!(
                r#"<tr><td>{}<div class="sub">added {} · {state}</div></td><td><form method="post" action="/notifications/remove"><input type="hidden" name="id" value="{id}"><button>Remove</button></form></td></tr>"#,
                esc(&label),
                esc(&app.clock.fmt(created)),
            );
        }
        body += "</table>";
    }
    body += "</div>";
    html::render(Page { title: "Notifications · baby-mcp", body, nav: Some(&user.display_name), refresh: None, viewer: false, footer_extra: String::new() })
}

#[derive(Deserialize)]
pub struct PrefBody {
    kind: String,
    enabled: bool,
}

/// One choice, saved as soon as its checkbox changes.
pub async fn save_pref(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<PrefBody>) -> Response {
    if !same_origin(&app, &headers) {
        return crate::auth::json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request refused");
    }
    let Some(user) = parent(&app, &headers) else { return crate::auth::json_err(StatusCode::UNAUTHORIZED, "unauthorized", "log in first (as a parent)") };
    if !KINDS.iter().any(|(k, _, _)| *k == b.kind) {
        return crate::auth::json_err(StatusCode::BAD_REQUEST, "bad_request", "unknown kind");
    }
    let r = app.db.with(|c| {
        c.execute(
            "INSERT INTO push_pref (user_id, kind, enabled) VALUES (?1, ?2, ?3) \
             ON CONFLICT (user_id, kind) DO UPDATE SET enabled = excluded.enabled",
            params![user.id, b.kind, b.enabled as i64],
        )
    });
    match r {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => crate::auth::json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RemoveForm {
    id: i64,
}

pub async fn remove(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<RemoveForm>) -> Response {
    if !same_origin(&app, &headers) {
        return html::error_page(StatusCode::FORBIDDEN, "Cross-site request refused.");
    }
    let user = match require(&app, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let _ = app.db.with(|c| c.execute("DELETE FROM push_subscription WHERE id = ?1 AND user_id = ?2", params![f.id, user.id]));
    Redirect::to("/notifications").into_response()
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    endpoint: String,
    keys: Option<SubscribeKeys>,
}

#[derive(Deserialize)]
pub struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

pub async fn subscribe(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<SubscribeBody>) -> Response {
    if !same_origin(&app, &headers) {
        return crate::auth::json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request refused");
    }
    let Some(user) = parent(&app, &headers) else { return crate::auth::json_err(StatusCode::UNAUTHORIZED, "unauthorized", "log in first (as a parent)") };
    let Some(keys) = b.keys else { return crate::auth::json_err(StatusCode::BAD_REQUEST, "bad_request", "subscription without keys") };
    if !b.endpoint.starts_with("https://") || b.endpoint.len() > 2048 || keys.p256dh.len() > 200 || keys.auth.len() > 100 {
        return crate::auth::json_err(StatusCode::BAD_REQUEST, "bad_request", "invalid subscription");
    }
    let label = device_label(&headers);
    let r = app.db.with(|c| {
        c.execute(
            "INSERT INTO push_subscription (user_id, endpoint, p256dh, auth, label, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (endpoint) DO UPDATE SET user_id = excluded.user_id, p256dh = excluded.p256dh, auth = excluded.auth",
            params![user.id, b.endpoint, keys.p256dh, keys.auth, label, now()],
        )
    });
    match r {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => crate::auth::json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UnsubscribeBody {
    endpoint: String,
}

pub async fn unsubscribe(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<UnsubscribeBody>) -> Response {
    if !same_origin(&app, &headers) {
        return crate::auth::json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request refused");
    }
    let Some(user) = parent(&app, &headers) else { return crate::auth::json_err(StatusCode::UNAUTHORIZED, "unauthorized", "log in first (as a parent)") };
    let _ = app.db.with(|c| c.execute("DELETE FROM push_subscription WHERE endpoint = ?1 AND user_id = ?2", params![b.endpoint, user.id]));
    Json(json!({ "ok": true })).into_response()
}

pub async fn test(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !same_origin(&app, &headers) {
        return crate::auth::json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request refused");
    }
    let Some(user) = parent(&app, &headers) else { return crate::auth::json_err(StatusCode::UNAUTHORIZED, "unauthorized", "log in first (as a parent)") };
    let n = Notice {
        kind: "test",
        key: String::new(),
        title: format!("🔔 {}: test", app.cfg.baby.name),
        body: "Notifications work on this device.".into(),
        url: "/notifications".into(),
        ttl: 600,
        by: None,
    };
    match notify::deliver(&app, &app.push_client, user.id, &n, "test").await {
        Ok(sent) => Json(json!({ "sent": sent })).into_response(),
        Err(e) => crate::auth::json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e),
    }
}

/// The text of a pushed message, fetched by the service worker. The id (only
/// in the encrypted push) is the capability; it expires after a day.
pub async fn message(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let hash = crate::auth::sha256_hex(&id);
    let r = app.db.with(|c| {
        rusqlite::OptionalExtension::optional(c.query_row(
            "SELECT title, body, url, tag FROM push_message WHERE hash = ?1 AND expires > ?2",
            params![hash, now()],
            |r| Ok(json!({ "title": r.get::<_, String>(0)?, "body": r.get::<_, String>(1)?, "url": r.get::<_, String>(2)?, "tag": r.get::<_, Option<String>>(3)? })),
        ))
    });
    match r {
        Ok(Some(mut v)) => {
            // The current badge, so the icon updates with the notification.
            if let Ok((items, _)) = notify::current(&app, now()) {
                v["badge"] = json!(items.len());
            }
            ([(header::CACHE_CONTROL, "no-store")], Json::<Value>(v)).into_response()
        }
        _ => crate::auth::json_err(StatusCode::NOT_FOUND, "not_found", "no such message"),
    }
}

/// The app icon's badge: count and what it counts (session required).
pub async fn badge(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if parent(&app, &headers).is_none() {
        return crate::auth::json_err(StatusCode::UNAUTHORIZED, "unauthorized", "log in first");
    }
    match notify::current(&app, now()) {
        Ok((items, groups)) => ([(header::CACHE_CONTROL, "no-store")], Json(json!({ "count": items.len(), "items": items, "groups": groups }))).into_response(),
        Err(e) => crate::auth::json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e),
    }
}

const SW_JS: &str = r#"// baby-mcp service worker: shows pushed notifications. No caching.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));
self.addEventListener('push', (e) => {
  e.waitUntil((async () => {
    let msg = {title: NAME, body: 'Open the app for details.', url: '/'};
    let group = 'baby-mcp';
    try {
      const id = e.data && e.data.json().m;
      if (id) {
        const r = await fetch('/push/message/' + encodeURIComponent(id), {cache: 'no-store', credentials: 'omit'});
        if (r.ok) msg = await r.json();
        group = msg.tag || msg.url;
      }
    } catch (_) {}
    try {
      if ('setAppBadge' in self.navigator && typeof msg.badge === 'number') {
        await (msg.badge ? self.navigator.setAppBadge(msg.badge) : self.navigator.clearAppBadge());
      }
    } catch (_) {}
    // Only show, one tag per group. Closing older ones from here is ignored
    // on iOS (tested: the worker sees them and calls close(), they stay);
    // the page closes stale ones when the app is opened.
    await self.registration.showNotification(msg.title, {body: msg.body, tag: group, renotify: true, icon: '/icon-192.png', data: {url: msg.url, group, at: Date.now()}});
  })());
});
self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  const url = (e.notification.data && e.notification.data.url) || '/';
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({type: 'window', includeUncontrolled: true});
    for (const w of wins) {
      try { await w.navigate(url); } catch (_) {}
      return w.focus();
    }
    return self.clients.openWindow(url);
  })());
});
"#;

pub async fn service_worker(State(app): State<Arc<App>>) -> Response {
    let js = format!("const NAME = {};\n{SW_JS}", serde_json::to_string(&app.cfg.baby.name).unwrap());
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], js).into_response()
}
