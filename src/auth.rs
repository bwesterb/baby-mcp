//! Authentication: passkeys and web sessions for people, and a minimal
//! OAuth 2.1 provider (dynamic client registration, PKCE S256, resource-bound
//! access tokens, rotating refresh tokens) so Claude can connect.

use crate::app::App;
use crate::entries::err;
use crate::html::{self, Page, esc};
use crate::time::now;
use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use webauthn_rs::prelude::*;

pub const SESSION_COOKIE: &str = "baby_session";
const SESSION_TTL: i64 = 30 * 24 * 3600;
const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 30 * 24 * 3600;
const ENROLL_TTL: i64 = 30 * 60;
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CLIENTS: i64 = 50;
pub const SCOPE: &str = "baby";

// ------------------------------------------------------------- helpers

pub fn random_token(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::fill(&mut b[..]);
    B64.encode(b)
}

pub fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn json_err(status: StatusCode, code: &str, desc: &str) -> Response {
    (status, Json(json!({"error": code, "error_description": desc}))).into_response()
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Browser POSTs must come from our own pages.
pub fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) == Some(app.cfg.origin())
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("authorization, content-type, mcp-protocol-version"),
    );
    r
}

pub async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn user_uuid(user_id: i64) -> Uuid {
    let d = Sha256::digest(format!("baby-mcp user {user_id}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    Uuid::from_bytes(b)
}

/// The credential id as base64url, the key passkeys are looked up by.
/// (It used to be derived via serde, which gave "" for every passkey.)
pub fn cred_key(id: &CredentialID) -> String {
    let bytes: &[u8] = id.as_ref();
    B64.encode(bytes)
}

/// Fills in `cred_id` for passkeys stored under the old, empty key.
pub fn fix_cred_ids(c: &rusqlite::Connection) -> rusqlite::Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT id, credential FROM passkey WHERE cred_id = ''")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut n = 0;
    for (id, cred) in rows {
        if let Ok(pk) = serde_json::from_str::<Passkey>(&cred) {
            n += c.execute("UPDATE passkey SET cred_id = ?1 WHERE id = ?2", params![cred_key(pk.cred_id()), id])?;
        }
    }
    Ok(n)
}

pub fn build_webauthn(origin: &str, host: &str) -> Result<Webauthn, String> {
    let url = Url::parse(origin).map_err(|e| e.to_string())?;
    WebauthnBuilder::new(host, &url)
        .and_then(|b| b.rp_name("baby-mcp").build())
        .map_err(|e| format!("webauthn: {e}"))
}

// ------------------------------------------------------------- in-memory state

pub enum Ceremony {
    Login { state: PasskeyAuthentication, next: String },
    Enroll { state: PasskeyRegistration, user_id: i64, token_hash: String, name: String },
}

pub struct PendingAuthz {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: String,
    challenge: String,
}

pub struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    user_id: i64,
}

#[derive(Default)]
pub struct Mem {
    pub ceremonies: HashMap<String, (Instant, Ceremony)>,
    pub authz: HashMap<String, (Instant, PendingAuthz)>,
    pub codes: HashMap<String, (Instant, Code)>,
}

impl Mem {
    fn gc(&mut self) {
        self.ceremonies.retain(|_, (t, _)| t.elapsed() < PENDING_TTL);
        self.authz.retain(|_, (t, _)| t.elapsed() < PENDING_TTL);
        self.codes.retain(|_, (t, _)| t.elapsed() < CODE_TTL);
    }
}

// ------------------------------------------------------------- sessions

pub struct User {
    pub id: i64,
    pub display_name: String,
    /// Only the charts, up to yesterday; nothing else, no changes, no MCP.
    pub viewer: bool,
}

pub fn session_user(app: &App, headers: &HeaderMap) -> Option<User> {
    let tok = cookie(headers, SESSION_COOKIE)?;
    app.db
        .with(|c| {
            c.query_row(
                "SELECT u.id, u.display_name, u.role = 'viewer' FROM session s JOIN user u ON u.id = s.user_id \
                 WHERE s.hash = ?1 AND s.expires > ?2",
                params![sha256_hex(&tok), now()],
                |r| Ok(User { id: r.get(0)?, display_name: r.get(1)?, viewer: r.get(2)? }),
            )
            .optional()
        })
        .ok()
        .flatten()
}

fn new_session(app: &App, user_id: i64) -> Result<String, String> {
    let tok = random_token(32);
    let t = now();
    app.db
        .with(|c| {
            c.execute("DELETE FROM session WHERE expires < ?1", [t])?;
            c.execute(
                "INSERT INTO session (hash, user_id, created_at, expires) VALUES (?1, ?2, ?3, ?4)",
                params![sha256_hex(&tok), user_id, t, t + SESSION_TTL],
            )
        })
        .map_err(err)?;
    Ok(format!(
        "{SESSION_COOKIE}={tok}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL}"
    ))
}

/// Redirects to the login page, coming back to `next` afterwards.
pub fn to_login(next: &str) -> Response {
    Redirect::to(&format!("/login?next={}", urlenc(next))).into_response()
}

pub fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn safe_next(next: Option<&String>) -> String {
    match next {
        // Browsers read a backslash as "/" in a URL, so "/\host" would leave the site.
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.clone(),
        _ => "/".into(),
    }
}

pub async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !same_origin(&app, &headers) {
        return html::error_page(StatusCode::FORBIDDEN, "Cross-site request refused.");
    }
    if let Some(tok) = cookie(&headers, SESSION_COOKIE) {
        let _ = app.db.with(|c| c.execute("DELETE FROM session WHERE hash = ?1", [sha256_hex(&tok)]));
    }
    let mut r = Redirect::to("/login").into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("baby_session=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0"),
    );
    r
}

// ------------------------------------------------------------- passkey login

pub async fn login_page(Query(q): Query<HashMap<String, String>>) -> Response {
    let next = safe_next(q.get("next"));
    html::render(Page {
        title: "Log in · baby-mcp",
        body: format!(
            r#"<div class="card"><h1>Log in</h1><p class="mut">Use a passkey enrolled for this baby log.</p>
<div class="row"><button class="primary" id="login" data-next="{}">Log in with passkey</button></div><div id="status"></div></div>"#,
            esc(&next)
        ),
        nav: None,
        refresh: None,
        viewer: false,
        footer_extra: String::new(),
    })
}

#[derive(Deserialize)]
pub struct LoginBegin {
    next: Option<String>,
}

pub async fn login_begin(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<LoginBegin>) -> Response {
    if !same_origin(&app, &headers) {
        return json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request");
    }
    let keys: Vec<Passkey> = match app.db.with(|c| {
        let mut st = c.prepare("SELECT credential FROM passkey")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }) {
        Ok(v) => v.iter().filter_map(|s| serde_json::from_str(s).ok()).collect(),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &err(e)),
    };
    if keys.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "no_passkeys", "no passkeys enrolled yet; run `baby-mcp user add`");
    }
    match app.wa.start_passkey_authentication(&keys) {
        Ok((opts, state)) => {
            let id = random_token(16);
            let mut m = app.mem.lock().unwrap();
            m.gc();
            m.ceremonies
                .insert(id.clone(), (Instant::now(), Ceremony::Login { state, next: safe_next(b.next.as_ref()) }));
            Json(json!({"id": id, "options": opts})).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct Finish {
    id: String,
    credential: Value,
}

pub async fn login_finish(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<Finish>) -> Response {
    if !same_origin(&app, &headers) {
        return json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request");
    }
    let cer = app.mem.lock().unwrap().ceremonies.remove(&b.id);
    let Some((_, Ceremony::Login { state, next })) = cer else {
        return json_err(StatusCode::BAD_REQUEST, "expired", "login expired; try again");
    };
    let cred: PublicKeyCredential = match serde_json::from_value(b.credential) {
        Ok(c) => c,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, "bad_credential", &e.to_string()),
    };
    let res = match app.wa.finish_passkey_authentication(&cred, &state) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("login: passkey verification failed: {e}");
            return json_err(StatusCode::FORBIDDEN, "denied", "passkey verification failed");
        }
    };
    let key = cred_key(res.cred_id());
    let found = app.db.with(|c| {
        c.query_row(
            "SELECT id, user_id, credential FROM passkey WHERE cred_id = ?1",
            [&key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()
    });
    let Ok(Some((pk_id, user_id, raw))) = found else {
        return json_err(StatusCode::FORBIDDEN, "denied", "unknown passkey");
    };
    if let Ok(mut pk) = serde_json::from_str::<Passkey>(&raw) {
        pk.update_credential(&res);
        let _ = app.db.with(|c| {
            c.execute(
                "UPDATE passkey SET credential = ?1, last_used = ?2 WHERE id = ?3",
                params![serde_json::to_string(&pk).unwrap_or(raw.clone()), now(), pk_id],
            )
        });
    }
    match new_session(&app, user_id) {
        Ok(set) => {
            eprintln!("login: user {user_id} with passkey {pk_id}");
            let mut r = Json(json!({"redirect": next})).into_response();
            r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&set).unwrap());
            r
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e),
    }
}

// ------------------------------------------------------------- enrollment

/// Creates a single-use enrollment token for a user (admin CLI).
pub fn new_enroll_token(c: &Connection, user_id: i64) -> rusqlite::Result<String> {
    let tok = random_token(24);
    c.execute("DELETE FROM enroll_token WHERE expires < ?1", [now()])?;
    c.execute(
        "INSERT INTO enroll_token (hash, user_id, expires) VALUES (?1, ?2, ?3)",
        params![sha256_hex(&tok), user_id, now() + ENROLL_TTL],
    )?;
    Ok(tok)
}

fn enroll_user(app: &App, token: &str) -> Option<(i64, String)> {
    app.db
        .with(|c| {
            c.query_row(
                "SELECT u.id, u.display_name FROM enroll_token t JOIN user u ON u.id = t.user_id \
                 WHERE t.hash = ?1 AND t.expires > ?2",
                params![sha256_hex(token), now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .ok()
        .flatten()
}

pub async fn enroll_page(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    let Some((_, display)) = enroll_user(&app, &token) else {
        return html::error_page(
            StatusCode::FORBIDDEN,
            "This enrollment link is invalid or expired. Ask for a new one (baby-mcp enroll NAME).",
        );
    };
    html::render(Page {
        title: "Add a passkey · baby-mcp",
        body: format!(
            r#"<div class="card"><h1>Add a passkey for {}</h1><p class="mut">Anyone with an enrolled passkey can read and write this baby log.</p>
<p><input id="name" placeholder="Name for this passkey, e.g. phone or laptop" maxlength="60"></p>
<div class="row"><button class="primary" id="enroll" data-token="{}">Create passkey</button></div><div id="status"></div></div>"#,
            esc(&display),
            esc(&token)
        ),
        nav: None,
        refresh: None,
        viewer: false,
        footer_extra: String::new(),
    })
}

#[derive(Deserialize)]
pub struct EnrollBegin {
    token: String,
    name: String,
}

pub async fn enroll_begin(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<EnrollBegin>) -> Response {
    if !same_origin(&app, &headers) {
        return json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request");
    }
    let name = b.name.trim().to_string();
    if name.is_empty() || name.len() > 60 {
        return json_err(StatusCode::BAD_REQUEST, "bad_name", "give the passkey a name of at most 60 characters");
    }
    let Some((user_id, display)) = enroll_user(&app, &b.token) else {
        return json_err(StatusCode::FORBIDDEN, "expired", "enrollment link invalid or expired");
    };
    let existing: Vec<CredentialID> = app
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT credential FROM passkey WHERE user_id = ?1")?;
            let rows = st.query_map([user_id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|s| serde_json::from_str::<Passkey>(s).ok())
        .map(|p| p.cred_id().clone())
        .collect();
    match app
        .wa
        .start_passkey_registration(user_uuid(user_id), &display, &display, Some(existing))
    {
        Ok((opts, state)) => {
            let id = random_token(16);
            let mut m = app.mem.lock().unwrap();
            m.gc();
            m.ceremonies.insert(
                id.clone(),
                (Instant::now(), Ceremony::Enroll { state, user_id, token_hash: sha256_hex(&b.token), name }),
            );
            Json(json!({"id": id, "options": opts})).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

pub async fn enroll_finish(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<Finish>) -> Response {
    if !same_origin(&app, &headers) {
        return json_err(StatusCode::FORBIDDEN, "forbidden", "cross-site request");
    }
    let cer = app.mem.lock().unwrap().ceremonies.remove(&b.id);
    let Some((_, Ceremony::Enroll { state, user_id, token_hash, name })) = cer else {
        return json_err(StatusCode::BAD_REQUEST, "expired", "enrollment expired; reload the page");
    };
    let cred: RegisterPublicKeyCredential = match serde_json::from_value(b.credential) {
        Ok(c) => c,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, "bad_credential", &e.to_string()),
    };
    let pk = match app.wa.finish_passkey_registration(&cred, &state) {
        Ok(p) => p,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, "denied", &format!("passkey verification failed: {e}")),
    };
    let stored = app.db.with(|c| {
        let tx = c.transaction()?;
        let n = tx.execute(
            "DELETE FROM enroll_token WHERE hash = ?1 AND expires > ?2",
            params![token_hash, now()],
        )?;
        if n == 1 {
            tx.execute(
                "INSERT INTO passkey (user_id, name, cred_id, credential, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user_id, name, cred_key(pk.cred_id()), serde_json::to_string(&pk).unwrap(), now()],
            )?;
        }
        tx.commit()?;
        Ok(n == 1)
    });
    match stored {
        Ok(true) => {}
        Ok(false) => return json_err(StatusCode::FORBIDDEN, "expired", "enrollment link was already used or expired"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &err(e)),
    }
    eprintln!("enroll: passkey for user {user_id}");
    match new_session(&app, user_id) {
        Ok(set) => {
            let mut r = Json(json!({"redirect": "/"})).into_response();
            r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&set).unwrap());
            r
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e),
    }
}

// ------------------------------------------------------------- OAuth: metadata & registration

fn resource(app: &App) -> String {
    format!("{}/mcp", app.cfg.origin())
}

pub async fn protected_resource(State(app): State<Arc<App>>) -> Response {
    cors(
        Json(json!({
            "resource": resource(&app),
            "authorization_servers": [app.cfg.origin()],
            "bearer_methods_supported": ["header"],
            "scopes_supported": [SCOPE],
            "resource_name": "baby-mcp",
        }))
        .into_response(),
    )
}

pub async fn authorization_server(State(app): State<Arc<App>>) -> Response {
    let o = app.cfg.origin();
    cors(
        Json(json!({
            "issuer": o,
            "authorization_endpoint": format!("{o}/oauth/authorize"),
            "token_endpoint": format!("{o}/oauth/token"),
            "registration_endpoint": format!("{o}/oauth/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
            "scopes_supported": [SCOPE],
        }))
        .into_response(),
    )
}

fn redirect_allowed(app: &App, raw: &str) -> bool {
    let Ok(u) = Url::parse(raw) else { return false };
    if u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    match u.scheme() {
        "http" => matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"),
        "https" => app.cfg.oauth_redirect_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)),
        _ => false,
    }
}

#[derive(Deserialize)]
pub struct RegisterReq {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

pub async fn register(State(app): State<Arc<App>>, body: axum::body::Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<RegisterReq>(&body) else {
        return cors(json_err(StatusCode::BAD_REQUEST, "invalid_client_metadata", "body must be JSON"));
    };
    if req.redirect_uris.is_empty() || req.redirect_uris.len() > 10 {
        return cors(json_err(StatusCode::BAD_REQUEST, "invalid_redirect_uri", "give 1 to 10 redirect_uris"));
    }
    if let Some(bad) = req.redirect_uris.iter().find(|u| !redirect_allowed(&app, u)) {
        return cors(json_err(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("redirect URI {bad:?} is not allowed by this server"),
        ));
    }
    let method = req.token_endpoint_auth_method.unwrap_or_else(|| "none".into());
    if !["none", "client_secret_post", "client_secret_basic"].contains(&method.as_str()) {
        return cors(json_err(StatusCode::BAD_REQUEST, "invalid_client_metadata", "unsupported token_endpoint_auth_method"));
    }
    let name: String = req.client_name.unwrap_or_default().trim().chars().take(100).collect();
    let name = if name.is_empty() { "unnamed client".to_string() } else { name };
    let id = format!("bmc_{}", random_token(16));
    let secret = (method != "none").then(|| random_token(32));
    let t = now();
    let res = app.db.with(|c| {
        // Keep at most MAX_CLIENTS: evict clients that never got a token
        // first, then the least recently used. Registration needs no login,
        // so anyone could otherwise push out the clients in real use.
        c.execute(
            "DELETE FROM oauth_client WHERE id IN (SELECT id FROM oauth_client \
             ORDER BY last_used IS NOT NULL DESC, coalesce(last_used, created_at) DESC LIMIT -1 OFFSET ?1)",
            [MAX_CLIENTS - 1],
        )?;
        c.execute(
            "INSERT INTO oauth_client (id, secret_hash, name, redirect_uris, auth_method, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, secret.as_deref().map(sha256_hex), name, json!(req.redirect_uris).to_string(), method, t],
        )
    });
    if let Err(e) = res {
        return cors(json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &err(e)));
    }
    eprintln!("oauth: registered client {id} ({name})");
    let mut out = json!({
        "client_id": id, "client_id_issued_at": t, "client_name": name,
        "redirect_uris": req.redirect_uris, "token_endpoint_auth_method": method,
        "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"], "scope": SCOPE,
    });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
        out["client_secret_expires_at"] = json!(0);
    }
    cors((StatusCode::CREATED, Json(out)).into_response())
}

struct Client {
    id: String,
    name: String,
    redirect_uris: Vec<String>,
    auth_method: String,
    secret_hash: Option<String>,
}

fn load_client(c: &Connection, id: &str) -> rusqlite::Result<Option<Client>> {
    c.query_row(
        "SELECT id, name, redirect_uris, auth_method, secret_hash FROM oauth_client WHERE id = ?1",
        [id],
        |r| {
            Ok(Client {
                id: r.get(0)?,
                name: r.get(1)?,
                redirect_uris: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                auth_method: r.get(3)?,
                secret_hash: r.get(4)?,
            })
        },
    )
    .optional()
}

// ------------------------------------------------------------- OAuth: authorize

fn with_params(base: &str, kv: &[(&str, &str)]) -> String {
    let mut u = match Url::parse(base) {
        Ok(u) => u,
        Err(_) => return base.to_string(),
    };
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in kv {
            if !v.is_empty() {
                q.append_pair(k, v);
            }
        }
    }
    u.to_string()
}

pub async fn authorize(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let client = match app.db.with(|c| load_client(c, get("client_id"))) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return html::error_page(StatusCode::BAD_REQUEST, "Unknown client. Remove the connector and add it again.");
        }
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &err(e)),
    };
    let mut redirect = get("redirect_uri").to_string();
    if redirect.is_empty() && client.redirect_uris.len() == 1 {
        redirect = client.redirect_uris[0].clone();
    }
    if !client.redirect_uris.contains(&redirect) {
        return html::error_page(StatusCode::BAD_REQUEST, "The redirect URI does not match the one this client registered.");
    }
    let iss = app.cfg.origin().to_string();
    let fail = |code: &str, desc: &str| {
        Redirect::to(&with_params(
            &redirect,
            &[("error", code), ("error_description", desc), ("state", get("state")), ("iss", &iss)],
        ))
        .into_response()
    };
    if get("response_type") != "code" {
        return fail("unsupported_response_type", "only response_type=code is supported");
    }
    if get("code_challenge").is_empty() || get("code_challenge_method") != "S256" {
        return fail("invalid_request", "PKCE with code_challenge_method=S256 is required");
    }
    let res = get("resource").trim_end_matches('/');
    if !res.is_empty() && res != resource(&app) {
        return fail("invalid_target", "unknown resource");
    }
    let Some(user) = session_user(&app, &headers) else {
        return to_login(&uri.to_string());
    };
    if user.viewer {
        return html::error_page(StatusCode::FORBIDDEN, "This account can only view the charts.");
    }
    let rid = random_token(16);
    {
        let mut m = app.mem.lock().unwrap();
        m.gc();
        m.authz.insert(
            rid.clone(),
            (
                Instant::now(),
                PendingAuthz {
                    client_id: client.id.clone(),
                    client_name: client.name.clone(),
                    redirect_uri: redirect.clone(),
                    state: get("state").to_string(),
                    challenge: get("code_challenge").to_string(),
                },
            ),
        );
    }
    let host = Url::parse(&redirect).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    html::render(Page {
        title: "Allow access? · baby-mcp",
        body: format!(
            r#"<div class="card"><h1>Allow access?</h1>
<p><strong>{name}</strong> wants to read and write this baby log as you ({me}).</p>
<p class="mut">You will be sent back to <code>{host}</code>.</p>
<form method="post" action="/oauth/authorize"><input type="hidden" name="rid" value="{rid}">
<div class="row"><button class="primary" name="decision" value="allow">Allow</button><button name="decision" value="deny">Deny</button></div></form></div>"#,
            name = esc(&client.name),
            me = esc(&user.display_name),
            host = esc(&host),
            rid = esc(&rid)
        ),
        nav: None,
        refresh: None,
        viewer: false,
        footer_extra: String::new(),
    })
}

#[derive(Deserialize)]
pub struct Decide {
    rid: String,
    decision: String,
}

pub async fn authorize_decide(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<Decide>) -> Response {
    if !same_origin(&app, &headers) {
        return html::error_page(StatusCode::FORBIDDEN, "Cross-site request refused.");
    }
    let Some(user) = session_user(&app, &headers).filter(|u| !u.viewer) else {
        return html::error_page(StatusCode::FORBIDDEN, "Not logged in (as a parent).");
    };
    let Some((_, p)) = app.mem.lock().unwrap().authz.remove(&f.rid) else {
        return html::error_page(StatusCode::BAD_REQUEST, "This request expired; start again from Claude.");
    };
    let iss = app.cfg.origin().to_string();
    if f.decision != "allow" {
        return Redirect::to(&with_params(
            &p.redirect_uri,
            &[("error", "access_denied"), ("state", &p.state), ("iss", &iss)],
        ))
        .into_response();
    }
    let code = random_token(32);
    app.mem.lock().unwrap().codes.insert(
        code.clone(),
        (
            Instant::now(),
            Code { client_id: p.client_id.clone(), redirect_uri: p.redirect_uri.clone(), challenge: p.challenge, user_id: user.id },
        ),
    );
    eprintln!("oauth: user {} approved client {} ({})", user.id, p.client_id, p.client_name);
    Redirect::to(&with_params(&p.redirect_uri, &[("code", &code), ("state", &p.state), ("iss", &iss)])).into_response()
}

// ------------------------------------------------------------- OAuth: token

fn signing_key(c: &Connection) -> rusqlite::Result<Vec<u8>> {
    if let Some(k) = c.query_row("SELECT value FROM kv WHERE key = 'signing_key'", [], |r| r.get(0)).optional()? {
        return Ok(k);
    }
    let mut k = vec![0u8; 32];
    rand::fill(&mut k[..]);
    c.execute("INSERT OR IGNORE INTO kv (key, value) VALUES ('signing_key', ?1)", [&k])?;
    c.query_row("SELECT value FROM kv WHERE key = 'signing_key'", [], |r| r.get(0))
}

pub fn rotate_signing_key(c: &Connection) -> rusqlite::Result<()> {
    c.execute("DELETE FROM kv WHERE key = 'signing_key'", [])?;
    signing_key(c).map(|_| ())
}

fn mac(key: &[u8], msg: &str) -> String {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac key");
    m.update(msg.as_bytes());
    B64.encode(m.finalize().into_bytes())
}

fn issue_access(key: &[u8], aud: &str, user_id: i64, client_id: &str) -> String {
    let t = now();
    let payload = json!({"aud": aud, "uid": user_id, "cid": client_id, "iat": t, "exp": t + ACCESS_TTL});
    let msg = format!("bm1.{}", B64.encode(payload.to_string()));
    let sig = mac(key, &msg);
    format!("{msg}.{sig}")
}

/// Returns the user id of a valid access token for this resource.
pub fn verify_access(app: &App, tok: &str) -> Result<i64, &'static str> {
    let (msg, sig) = tok.rsplit_once('.').ok_or("malformed token")?;
    let payload = msg.strip_prefix("bm1.").ok_or("malformed token")?;
    let key = app.db.with(|c| signing_key(c)).map_err(|_| "server error")?;
    let expect = mac(&key, msg);
    if expect.len() != sig.len() || !bool::from(subtle_eq(expect.as_bytes(), sig.as_bytes())) {
        return Err("bad signature");
    }
    let v: Value = B64
        .decode(payload)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("malformed token")?;
    if v["exp"].as_i64().unwrap_or(0) <= now() {
        return Err("token expired");
    }
    if v["aud"].as_str() != Some(&resource(app)) {
        return Err("token is for another resource");
    }
    v["uid"].as_i64().ok_or("malformed token")
}

fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    (43..=128).contains(&verifier.len()) && subtle_eq(B64.encode(Sha256::digest(verifier.as_bytes())).as_bytes(), challenge.as_bytes())
}

pub async fn token(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> Response {
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
    let (mut cid, mut secret) = (get("client_id").to_string(), get("client_secret").to_string());
    if let Some((u, p)) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').map(|(a, b)| (a.to_string(), b.to_string())))
    {
        cid = Url::parse(&format!("x:?a={u}&b={p}"))
            .ok()
            .and_then(|x| x.query_pairs().find(|(k, _)| k == "a").map(|(_, v)| v.into_owned()))
            .unwrap_or(u);
        secret = Url::parse(&format!("x:?b={p}"))
            .ok()
            .and_then(|x| x.query_pairs().next().map(|(_, v)| v.into_owned()))
            .unwrap_or(p);
    }
    let r = app.db.with(|c| -> rusqlite::Result<Result<Value, (StatusCode, &'static str, String)>> {
        let Some(client) = load_client(c, &cid)? else {
            return Ok(Err((StatusCode::UNAUTHORIZED, "invalid_client", "unknown client".into())));
        };
        if client.auth_method != "none"
            && !subtle_eq(sha256_hex(&secret).as_bytes(), client.secret_hash.unwrap_or_default().as_bytes())
        {
            return Ok(Err((StatusCode::UNAUTHORIZED, "invalid_client", "bad client secret".into())));
        }
        let res = resource(&app);
        if !get("resource").is_empty() && get("resource").trim_end_matches('/') != res {
            return Ok(Err((StatusCode::BAD_REQUEST, "invalid_target", "unknown resource".into())));
        }
        let user_id = match get("grant_type") {
            "authorization_code" => {
                let code = app.mem.lock().unwrap().codes.remove(get("code"));
                let Some((t, code)) = code.filter(|(t, _)| t.elapsed() < CODE_TTL) else {
                    return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "unknown or expired code".into())));
                };
                let _ = t;
                if code.client_id != cid {
                    return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "code was issued to another client".into())));
                }
                if !get("redirect_uri").is_empty() && get("redirect_uri") != code.redirect_uri {
                    return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "redirect_uri mismatch".into())));
                }
                if !pkce_ok(get("code_verifier"), &code.challenge) {
                    return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "PKCE verification failed".into())));
                }
                code.user_id
            }
            "refresh_token" => {
                let h = sha256_hex(get("refresh_token"));
                let row: Option<(String, i64, i64)> = c
                    .query_row(
                        "SELECT client_id, user_id, expires FROM refresh_token WHERE hash = ?1",
                        [&h],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                match row {
                    Some((rc, uid, exp)) if rc == cid && exp > now() => {
                        c.execute("DELETE FROM refresh_token WHERE hash = ?1", [&h])?; // rotate
                        uid
                    }
                    Some((rc, _, _)) if rc != cid => {
                        return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "refresh token was issued to another client".into())));
                    }
                    _ => return Ok(Err((StatusCode::BAD_REQUEST, "invalid_grant", "unknown or expired refresh token".into()))),
                }
            }
            _ => return Ok(Err((StatusCode::BAD_REQUEST, "unsupported_grant_type", "use authorization_code or refresh_token".into()))),
        };
        let refresh = random_token(32);
        let t = now();
        c.execute("DELETE FROM refresh_token WHERE expires < ?1", [t])?;
        c.execute(
            "INSERT INTO refresh_token (hash, client_id, user_id, created_at, expires) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![sha256_hex(&refresh), cid, user_id, t, t + REFRESH_TTL],
        )?;
        c.execute("UPDATE oauth_client SET last_used = ?1 WHERE id = ?2", params![t, cid])?;
        let key = signing_key(c)?;
        Ok(Ok(json!({
            "access_token": issue_access(&key, &res, user_id, &cid), "token_type": "Bearer",
            "expires_in": ACCESS_TTL, "refresh_token": refresh, "scope": SCOPE,
        })))
    });
    let mut resp = match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((s, code, desc))) => json_err(s, code, &desc),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &err(e)),
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    cors(resp)
}

/// The 401 that points MCP clients at the OAuth metadata.
pub fn unauthorized(app: &App, why: &str, invalid: bool) -> Response {
    let mut chal = format!(
        r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource/mcp", scope="{SCOPE}""#,
        app.cfg.origin()
    );
    if invalid {
        chal.push_str(r#", error="invalid_token""#);
    }
    let mut r = json_err(StatusCode::UNAUTHORIZED, if invalid { "invalid_token" } else { "unauthorized" }, why);
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_str(&chal).unwrap());
    cors(r)
}

pub fn bearer_user(app: &App, headers: &HeaderMap) -> Result<i64, Response> {
    let tok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    match tok {
        None => Err(unauthorized(app, "missing bearer token", false)),
        Some(t) => {
            let uid = verify_access(app, t).map_err(|e| unauthorized(app, e, true))?;
            // A viewer (only the charts) has no MCP access, even with an older token.
            let viewer = app.db.with(|c| c.query_row("SELECT role = 'viewer' FROM user WHERE id = ?1", [uid], |r| r.get::<_, bool>(0))).unwrap_or(true);
            if viewer {
                return Err(unauthorized(app, "this account can only view the charts", true));
            }
            Ok(uid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_keys_differ() {
        let a = cred_key(&CredentialID::from(vec![1u8, 2, 3]));
        let b = cred_key(&CredentialID::from(vec![4u8, 5, 6]));
        assert_eq!(a, "AQID");
        assert_ne!(a, b);
    }
}
