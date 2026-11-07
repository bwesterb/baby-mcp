//! End-to-end tests of the HTTP layer: OAuth, MCP tools, the `sql` tool's
//! guard rails, and the web pages. (Passkey ceremonies are left to
//! webauthn-rs; a session is created directly.)

use crate::app::App;
use crate::auth::{SESSION_COOKIE, random_token, sha256_hex};
use crate::config::Config;
use crate::time::now;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;

const ORIGIN: &str = "https://baby.example.com";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

struct T {
    _dir: tempfile::TempDir,
    app: Arc<App>,
    cookie: String,
    /// Birth time: a week before the test runs, so age-dependent views apply.
    birth: i64,
}

fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let birth = now() - 7 * 86400;
    let cfg: Config = toml::from_str(&format!(
        r#"origin = "{ORIGIN}"
state = "{}"
[baby]
name = "Baby"
birth = {}
birth_weight_g = 3900
sex = "female"
delivery = "cesarean""#,
        dir.path().display(),
        jiff::Timestamp::from_second(birth).unwrap()
    ))
    .unwrap();
    // The WHO table is not in the repository; use it when it has been downloaded.
    let table = concat!(env!("CARGO_MANIFEST_DIR"), "/data/who-weight-for-age.tsv");
    let have_table = std::path::Path::new(table).exists();
    if have_table {
        std::fs::copy(table, dir.path().join("who-weight-for-age.tsv")).unwrap();
    }
    let change = concat!(env!("CARGO_MANIFEST_DIR"), "/data/newborn-weight-change.tsv");
    if std::path::Path::new(change).exists() {
        std::fs::copy(change, dir.path().join("newborn-weight-change.tsv")).unwrap();
    }
    let app = Arc::new(App::new(cfg).unwrap());
    assert_eq!(app.growth.is_some(), have_table);
    let tok = random_token(32);
    app.db
        .with(|c| {
            c.execute("INSERT INTO user (name, display_name, created_at) VALUES ('bas', 'Bas', 0)", [])?;
            c.execute(
                "INSERT INTO session (hash, user_id, created_at, expires) VALUES (?1, 1, 0, ?2)",
                rusqlite::params![sha256_hex(&tok), now() + 3600],
            )
        })
        .unwrap();
    T { _dir: dir, app, cookie: format!("{SESSION_COOKIE}={tok}"), birth }
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    fn location(&self) -> String {
        self.headers.get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }
}

async fn send(t: &T, req: Request<Body>) -> Resp {
    let r = crate::router(t.app.clone()).oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let body = String::from_utf8(to_bytes(r.into_body(), 1 << 22).await.unwrap().to_vec()).unwrap();
    Resp { status, headers, body }
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path)
}

fn post_json(path: &str, v: Value) -> Request<Body> {
    Request::post(path).header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap()
}

fn post_form(path: &str, body: &str) -> axum::http::request::Builder {
    let _ = body;
    Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
}

/// Runs the OAuth flow and returns (client_id, access token, refresh token).
async fn connect(t: &T) -> (String, String, String) {
    let reg = send(t, post_json("/oauth/register", json!({"redirect_uris": [REDIRECT], "client_name": "Claude"}))).await;
    assert_eq!(reg.status, StatusCode::CREATED, "{}", reg.body);
    let cid = reg.json()["client_id"].as_str().unwrap().to_string();
    let verifier = random_token(40);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let q = format!(
        "/oauth/authorize?response_type=code&client_id={cid}&redirect_uri={}&state=xyz&code_challenge={challenge}&code_challenge_method=S256&resource={}",
        crate::auth::urlenc(REDIRECT),
        crate::auth::urlenc(&format!("{ORIGIN}/mcp"))
    );
    // Without a session: to the login page.
    let r = send(t, get(&q).body(Body::empty()).unwrap()).await;
    assert!(r.location().starts_with("/login?next="), "{}", r.location());
    // With a session: the consent page.
    let r = send(t, get(&q).header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    let rid = r.body.split(r#"name="rid" value=""#).nth(1).unwrap().split('"').next().unwrap().to_string();
    // Cross-site approval is refused.
    let body = format!("rid={}&decision=allow", crate::auth::urlenc(&rid));
    let r = send(t, post_form("/oauth/authorize", &body).header(header::COOKIE, &t.cookie).header(header::ORIGIN, "https://evil.example").body(Body::from(body.clone())).unwrap()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(t, post_form("/oauth/authorize", &body).header(header::COOKIE, &t.cookie).header(header::ORIGIN, ORIGIN).body(Body::from(body)).unwrap()).await;
    assert!(r.status.is_redirection());
    let loc = webauthn_rs::prelude::Url::parse(&r.location()).unwrap();
    assert!(r.location().starts_with(REDIRECT));
    let code = loc.query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string();
    assert_eq!(loc.query_pairs().find(|(k, _)| k == "state").unwrap().1, "xyz");
    // A wrong verifier burns the code.
    let form = |v: &str, c: &str| format!("grant_type=authorization_code&code={}&client_id={cid}&redirect_uri={}&code_verifier={v}", crate::auth::urlenc(c), crate::auth::urlenc(REDIRECT));
    let tok = send(t, post_form("/oauth/token", "").body(Body::from(form(&verifier, &code))).unwrap()).await;
    assert_eq!(tok.status, StatusCode::OK, "{}", tok.body);
    let again = send(t, post_form("/oauth/token", "").body(Body::from(form(&verifier, &code))).unwrap()).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "code reusable");
    let j = tok.json();
    (cid, j["access_token"].as_str().unwrap().into(), j["refresh_token"].as_str().unwrap().into())
}

async fn mcp(t: &T, tok: &str, method: &str, params: Value) -> Resp {
    send(
        t,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::from(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()))
            .unwrap(),
    )
    .await
}

async fn tool(t: &T, tok: &str, name: &str, args: Value) -> (bool, Value) {
    let r = mcp(t, tok, "tools/call", json!({"name": name, "arguments": args})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let res = &r.json()["result"];
    let err = res["isError"].as_bool().unwrap_or(false);
    let v = if err { json!(res["content"][0]["text"]) } else { res["structuredContent"].clone() };
    (err, v)
}

#[tokio::test]
async fn oauth_and_mcp() {
    let t = setup();
    // Discovery.
    let r = send(&t, Request::post("/mcp").body(Body::from("{}")).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers[header::WWW_AUTHENTICATE].to_str().unwrap()
        .contains(&format!(r#"resource_metadata="{ORIGIN}/.well-known/oauth-protected-resource/mcp""#)));
    let prm = send(&t, get("/.well-known/oauth-protected-resource/mcp").body(Body::empty()).unwrap()).await.json();
    assert_eq!(prm["resource"], format!("{ORIGIN}/mcp"));
    let asm = send(&t, get("/.well-known/oauth-authorization-server").body(Body::empty()).unwrap()).await.json();
    assert_eq!(asm["issuer"], ORIGIN);
    // Foreign redirect URIs are refused.
    let r = send(&t, post_json("/oauth/register", json!({"redirect_uris": ["https://evil.example/cb"]}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let (cid, access, refresh) = connect(&t).await;

    // MCP basics.
    let init = mcp(&t, &access, "initialize", json!({"protocolVersion": "2024-06-19"})).await.json();
    assert_eq!(init["result"]["protocolVersion"], "2024-06-19");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Baby"));
    let tools = mcp(&t, &access, "tools/list", json!({})).await.json();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 12);
    let bad = send(&t, Request::post("/mcp").header(header::AUTHORIZATION, format!("Bearer {}x", &access[..access.len() - 1])).body(Body::from("{}")).unwrap()).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED, "tampered token accepted");

    // log, duplicate detection, update, history.
    let (e, v) = tool(&t, &access, "log", json!({"text": "60 ml gekolfd, natte luier", "at": "now",
        "fields": {"bottle_breastmilk_ml": 60, "wet": true, "dirty": null}})).await;
    assert!(!e && v["logged"] == true, "{v}");
    let id = v["entry"]["id"].as_i64().unwrap();
    let (_, v) = tool(&t, &access, "log", json!({"text": "wet diaper", "fields": {"wet": true}})).await;
    assert_eq!(v["logged"], false, "duplicate not detected: {v}");
    let (_, v) = tool(&t, &access, "log", json!({"text": "wet diaper", "fields": {"wet": true}, "confirm": true})).await;
    assert_eq!(v["logged"], true);
    let (e, v) = tool(&t, &access, "log", json!({"text": "x", "fields": {"temp_c": 368}})).await;
    assert!(e, "implausible value accepted: {v}");
    let (e, _) = tool(&t, &access, "log", json!({"text": "x", "fields": {"mood": "happy"}})).await;
    assert!(e, "unknown field accepted");
    // Partial information: logged now, flagged for the parents, with a reason.
    let partial = json!({"text": "drinking at the breast now", "fields": {"latch": "with_aid"}, "needs_attention_by": ["user"], "confirm": true});
    let (e, v) = tool(&t, &access, "log", partial.clone()).await;
    assert!(e && v.as_str().unwrap().contains("needs_attention_why is required"), "{v}");
    let mut with_why = partial;
    with_why["needs_attention_why"] = json!("still feeding: add duration and amount");
    let (e, v) = tool(&t, &access, "log", with_why).await;
    assert!(!e && v["logged"] == true, "{v}");
    assert_eq!(v["entry"]["needs_attention"], json!({"by": ["user"], "why": "still feeding: add duration and amount"}));
    let (e, v) = tool(&t, &access, "update", json!({"entry_id": id, "fields": {"bottle_formula_ml": 20}, "remove": ["dirty"]})).await;
    assert!(!e, "{v}");
    assert_eq!(v["entry"]["fields"], json!({"bottle_breastmilk_ml": 60.0, "bottle_formula_ml": 20.0, "wet": true}));
    let (_, v) = tool(&t, &access, "history", json!({"entry_id": id})).await;
    assert_eq!(v["versions"].as_array().unwrap().len(), 2);
    let (_, v) = tool(&t, &access, "status", json!({})).await;
    assert_eq!(v["today"]["total_ml"], 80.0);
    assert_eq!(v["today"]["wet"], 2);
    let (_, v) = tool(&t, &access, "stats", json!({})).await;
    assert_eq!(v["days"].as_array().unwrap().len(), 7);
    let (_, v) = tool(&t, &access, "delete", json!({"entry_id": id})).await;
    assert_eq!(v["entry"]["deleted"], true);
    let (_, v) = tool(&t, &access, "status", json!({})).await;
    assert_eq!(v["today"]["total_ml"], 0.0);
    tool(&t, &access, "undelete", json!({"entry_id": id})).await;

    // sql: reads the log, but not credentials, and never writes.
    let (e, v) = tool(&t, &access, "sql", json!({"query": "SELECT sum(bottle_breastmilk_ml) AS ml FROM entry WHERE deleted = 0"})).await;
    assert!(!e && v["rows"][0]["ml"] == 60.0, "{v}");
    for q in ["SELECT * FROM kv", "SELECT * FROM passkey", "SELECT hash FROM session",
              "SELECT * FROM user", "DELETE FROM entry", "UPDATE entry SET text = 'x'",
              "ATTACH DATABASE '/tmp/x' AS x"] {
        let (e, v) = tool(&t, &access, "sql", json!({"query": q})).await;
        assert!(e, "sql allowed {q}: {v}");
    }
    let (e, _) = tool(&t, &access, "sql", json!({"query": "SELECT id, display_name FROM user"})).await;
    assert!(!e);

    // attention, suggest, fields.
    let (_, v) = tool(&t, &access, "fields", json!({})).await;
    assert!(v["fields"].as_array().unwrap().len() >= 12);
    let (_, v) = tool(&t, &access, "suggest", json!({"text": "wanted a sleep tracker"})).await;
    assert!(v["result"].as_str().unwrap().contains("recorded"));

    // Refresh tokens rotate.
    let form = format!("grant_type=refresh_token&refresh_token={}&client_id={cid}", crate::auth::urlenc(&refresh));
    let r = send(&t, post_form("/oauth/token", "").body(Body::from(form.clone())).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r2 = send(&t, post_form("/oauth/token", "").body(Body::from(form)).unwrap()).await;
    assert_eq!(r2.status, StatusCode::BAD_REQUEST, "refresh token reusable");
    let new_access = r.json()["access_token"].as_str().unwrap().to_string();
    assert_eq!(mcp(&t, &new_access, "ping", json!({})).await.status, StatusCode::OK);

    // Rotating the signing key revokes access tokens.
    t.app.db.with(|c| crate::auth::rotate_signing_key(c)).unwrap();
    assert_eq!(mcp(&t, &new_access, "ping", json!({})).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn web_pages() {
    let t = setup();
    let r = send(&t, get("/").body(Body::empty()).unwrap()).await;
    assert!(r.location().starts_with("/login"), "status page without session");
    let r = send(&t, get("/").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Baby") && r.body.contains("Nothing logged yet today"));
    assert!(r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("frame-ancestors 'none'"));
    // With no-referrer, browsers send `Origin: null` on form posts and every
    // button would be refused as cross-site.
    assert_eq!(r.headers["referrer-policy"], "same-origin");

    // Installable as a home-screen app: manifest and icons without a session.
    assert!(r.body.contains(r#"rel="manifest" href="/manifest.webmanifest""#) && r.body.contains("apple-touch-icon"));
    assert!(r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("manifest-src 'self'"));
    // CSS and JS are versioned static files; nothing inline, so a strict CSP.
    let csp = r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("style-src 'self'") && !csp.contains("unsafe-inline") && !csp.contains("nonce"));
    assert!(!r.body.contains("<style") && !r.body.contains("style=\"") && !r.body.contains("<script>"));
    let v = r.body.split("/app.css?v=").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert!(r.body.contains(&format!(r#"<script src="/app.js?v={v}" defer>"#)));
    for (path, ct) in [("/app.css", "text/css"), ("/app.js", "text/javascript")] {
        let a = send(&t, get(&format!("{path}?v={v}")).body(Body::empty()).unwrap()).await;
        assert!(a.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with(ct));
        assert!(a.headers[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    }
    let l = send(&t, get("/login?next=/charts").body(Body::empty()).unwrap()).await;
    assert!(l.body.contains(r#"id="login" data-next="/charts""#), "{}", l.body);
    let m = send(&t, get("/manifest.webmanifest").body(Body::empty()).unwrap()).await;
    assert_eq!(m.status, StatusCode::OK);
    assert_eq!(m.json()["display"], "standalone");
    assert_eq!(m.json()["name"], "Baby");
    for icon in ["/icon-180.png", "/icon-192.png", "/icon-512.png", "/icon-maskable-512.png", "/favicon.ico", "/elephant.png"] {
        let r = crate::router(t.app.clone()).oneshot(get(icon).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{icon}");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/png");
        let b = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(b.starts_with(b"\x89PNG"), "{icon}");
    }

    // Free text: stored and marked for an agent. A common entry, unedited:
    // fields known straight away.
    let body = "text=60+ml+kunstvoeding+om+3+uur";
    let r = send(&t, post_form("/add", body).header(header::COOKIE, &t.cookie).header(header::ORIGIN, ORIGIN).body(Body::from(body)).unwrap()).await;
    assert!(r.status.is_redirection(), "{}", r.body);
    let body = "text=Wet+diaper";
    send(&t, post_form("/add", body).header(header::COOKIE, &t.cookie).header(header::ORIGIN, ORIGIN).body(Body::from(body)).unwrap()).await;
    let r = send(&t, post_form("/add", body).header(header::COOKIE, &t.cookie).header(header::ORIGIN, "https://evil.example").body(Body::from(body)).unwrap()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "cross-site add accepted");
    let r = send(&t, get("/add").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(!r.body.contains("Common entries") && !r.body.contains(r#"name="at""#));
    let (n, attention, wet): (i64, i64, i64) = t.app.db.with(|c| c.query_row(
        "SELECT count(*), sum(needs_attention_by = 2), sum(coalesce(wet, 0)) FROM entry", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))).unwrap();
    assert_eq!((n, attention, wet), (2, 2, 0), "every web entry goes to an agent");
    let r = send(&t, get("/").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    // Entries needing attention: once each, in their own card first (not in the log), with the reason.
    assert!(r.body.contains("Needs attention") && r.body.find("Needs attention") < r.body.find("What's next") && r.body.contains("🤖") && r.body.matches("process the entries that need attention").count() == 1, "{}", r.body);
    assert_eq!(r.body.matches("fields not extracted yet").count(), 2, "{}", r.body);
    let r = send(&t, get("/charts").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(r.status == StatusCode::OK && r.body.contains("<svg"));
    if t.app.growth.is_some() {
        assert!(r.body.contains("WHO weight-for-age") && r.body.contains("<polygon"), "growth bands missing");
    }
    let r2 = send(&t, get("/charts").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(!r2.body.contains("🌡️ Temperature"), "no temperature chart without readings");
    for v in ["36.9", "38.1"] {
        t.app.db.with(|c| Ok(crate::entries::insert(c, 1, &crate::entries::NewEntry {
            at: now() - 3600, duration_s: None, text: format!("temp {v}"),
            fields: serde_json::from_str(&format!(r#"{{"temp_c": {v}}}"#)).unwrap(), attention_by: 0, attention_why: None }, now()))).unwrap().unwrap();
    }
    let r2 = send(&t, get("/charts").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(r2.body.contains("🌡️ Temperature") && r2.body.contains("38.1 °C") && r2.body.contains("#c0503e"), "temperature chart missing");
    if t.app.change.is_some() {
        assert!(r.body.contains("Weight change since birth") && r.body.contains("a C-section"), "weight-change chart missing");
    }
    // Status: last and next grouped per kind; previous days.
    let r = send(&t, get("/").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(r.body.contains("What's next") && r.body.contains("Wet diaper") && r.body.contains("← Yesterday"));
    assert!(!r.body.contains("🍼 Feeding guide"), "the feeding guide card was removed");
    // Sleeping side: after "facing left", the feed tile says the next side is right.
    t.app.db.with(|c| Ok(crate::entries::insert(c, 1, &crate::entries::NewEntry {
        at: now() - 60, duration_s: None, text: "Put down facing left".into(),
        fields: serde_json::from_str(r#"{"sleep_facing": "left"}"#).unwrap(), attention_by: 0, attention_why: None }, now()))).unwrap().unwrap();
    let r = send(&t, get("/").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(r.body.contains("😴 face ➡️ right") && !r.body.contains("(last ⬅️"), "next sleeping side missing");
    // Day 3 of life: the birth date plus two days.
    let clock = &t.app.clock;
    let day3 = clock.zoned(t.birth).date().checked_add(jiff::Span::new().days(2)).unwrap();
    let r = send(&t, get(&format!("/day?date={day3}")).header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    let heading = day3.strftime("%A %-d %B %Y").to_string();
    assert!(r.body.contains(&heading) && r.body.contains("day 3 of life") && r.body.contains("Nothing logged this day"), "{heading}");
    let (prev, next) = (day3.yesterday().unwrap(), day3.tomorrow().unwrap());
    assert!(r.body.contains(&format!(r#"href="/day?date={prev}""#)) && r.body.contains(&format!(r#"href="/day?date={next}""#)));
    let r = send(&t, get("/day?date=2099-01-01").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.location(), "/", "future day should go to today");
}

/// Decrypts an aes128gcm Web Push body as the browser would (RFC 8291).
fn push_decrypt(ua: &p256::SecretKey, auth: &[u8], body: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, KeyInit};
    use p256::elliptic_curve::sec1::ToSec1Point;
    let (salt, rest) = body.split_at(16);
    let idlen = rest[4] as usize;
    let (as_public, ct) = rest[5..].split_at(idlen);
    let ua_public = ua.public_key().to_sec1_point(false).as_bytes().to_vec();
    let shared = p256::ecdh::diffie_hellman(ua.to_nonzero_scalar(), p256::PublicKey::from_sec1_bytes(as_public).unwrap().as_affine());
    let h = crate::push::hmac;
    let ikm = h(&h(auth, &[shared.raw_secret_bytes().as_slice()]), &[b"WebPush: info\0", &ua_public, as_public, &[1]]);
    let prk = h(salt, &[&ikm]);
    let cek = &h(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]])[..16];
    let nonce: [u8; 12] = h(&prk, &[b"Content-Encoding: nonce\0", &[1]])[..12].try_into().unwrap();
    let mut pt = aes_gcm::Aes128Gcm::new_from_slice(cek).unwrap().decrypt((&nonce).into(), ct).unwrap();
    assert_eq!(pt.pop(), Some(2), "last-record delimiter");
    pt
}

#[tokio::test]
async fn push_notifications() {
    use std::sync::Mutex;
    let t = setup();
    // A fake push service: records requests; /gone answers 410.
    let seen: Arc<Mutex<Vec<(axum::http::HeaderMap, Vec<u8>)>>> = Arc::default();
    let s2 = seen.clone();
    let fake = axum::Router::new()
        .route("/ok", axum::routing::post(move |h: axum::http::HeaderMap, b: axum::body::Bytes| {
            let s = s2.clone();
            async move {
                s.lock().unwrap().push((h, b.to_vec()));
                StatusCode::CREATED
            }
        }))
        .route("/gone", axum::routing::post(|| async { StatusCode::GONE }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

    // The page: key for the browser, choices on by default.
    let r = send(&t, get("/notifications").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains(r#"data-key="B"#) && r.body.matches(" checked").count() == 5, "{}", r.body);
    let (f, e, c) = (r.body.find("🍼 Feeding").unwrap(), r.body.find("📝 Entries").unwrap(), r.body.find("💊 Care").unwrap());
    assert!(f < r.body.find(r#"data-kind="feed_warning""#).unwrap() && r.body.find(r#"data-kind="feed_warning""#).unwrap() < e && e < r.body.find(r#"data-kind="changes""#).unwrap() && r.body.find(r#"data-kind="changes""#).unwrap() < c);
    assert!(r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("worker-src 'self'"));

    // Subscribing: same-origin and logged in only; https endpoints only.
    let ua = crate::push::random_secret();
    let ua_pub = {
        use p256::elliptic_curve::sec1::ToSec1Point;
        B64.encode(ua.public_key().to_sec1_point(false).as_bytes())
    };
    let auth = B64.encode([7u8; 16]);
    let sub = |ep: &str| json!({"endpoint": ep, "keys": {"p256dh": ua_pub, "auth": auth}});
    let req = |v: Value, origin: &str, cookie: bool| {
        let mut b = Request::post("/push/subscribe").header(header::CONTENT_TYPE, "application/json").header(header::ORIGIN, origin);
        if cookie {
            b = b.header(header::COOKIE, &t.cookie);
        }
        b.body(Body::from(v.to_string())).unwrap()
    };
    assert_eq!(send(&t, req(sub("https://push.example/x"), "https://evil.example", true)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&t, req(sub("https://push.example/x"), ORIGIN, false)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&t, req(sub("http://push.example/x"), ORIGIN, true)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(send(&t, req(sub("https://push.example/x"), ORIGIN, true)).await.status, StatusCode::OK);
    let r = send(&t, Request::post("/push/unsubscribe").header(header::CONTENT_TYPE, "application/json").header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &t.cookie).body(Body::from(json!({"endpoint": "https://push.example/x"}).to_string())).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    // The fake service is plain http on localhost, so put those in directly.
    t.app.db.with(|c| {
        for p in ["ok", "gone"] {
            c.execute("INSERT INTO push_subscription (user_id, endpoint, p256dh, auth, label, created_at) VALUES (1, ?1, ?2, ?3, 'test', 0)",
                rusqlite::params![format!("http://{addr}/{p}"), ua_pub, auth])?;
        }
        Ok(())
    }).unwrap();
    assert_eq!(t.app.db.with(|c| c.query_row("SELECT count(*) FROM push_subscription", [], |r| r.get::<_, i64>(0))).unwrap(), 2);

    // Choices.
    let pref = |v: Value, origin: &str| Request::post("/push/prefs").header(header::CONTENT_TYPE, "application/json").header(header::ORIGIN, origin)
        .header(header::COOKIE, &t.cookie).body(Body::from(v.to_string())).unwrap();
    assert_eq!(send(&t, pref(json!({"kind": "feed_overdue", "enabled": false}), "https://evil.example")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&t, pref(json!({"kind": "nonsense", "enabled": false}), ORIGIN)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(send(&t, pref(json!({"kind": "feed_overdue", "enabled": false}), ORIGIN)).await.status, StatusCode::OK);
    t.app.db.with(|c| {
        assert!(crate::notify::wants(c, 1, "feed_soon")? && !crate::notify::wants(c, 1, "feed_overdue")? && crate::notify::wants(c, 1, "attention")?);
        Ok(())
    }).unwrap();

    // A test push: encrypted, VAPID-signed, carrying only a message id.
    let r = send(&t, Request::post("/push/test").header(header::ORIGIN, ORIGIN).header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.json()["sent"], 1, "{}", r.body);
    assert_eq!(t.app.db.with(|c| c.query_row("SELECT count(*) FROM push_subscription", [], |r| r.get::<_, i64>(0))).unwrap(), 1, "410 removes it");
    let (h, b) = seen.lock().unwrap().pop().unwrap();
    assert_eq!(h["content-encoding"], "aes128gcm");
    assert_eq!(h["topic"], crate::push::topic("test").as_str(), "a valid base64url topic");
    assert!(h["authorization"].to_str().unwrap().starts_with("vapid t=ey"));
    let pt: Value = serde_json::from_slice(&push_decrypt(&ua, &[7u8; 16], &b)).unwrap();
    let id = pt["m"].as_str().unwrap();
    assert_eq!(pt.as_object().unwrap().len(), 1, "nothing else in the push");
    let r = send(&t, get(&format!("/push/message/{id}")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["title"], "🔔 Baby: test");
    assert!(r.json()["badge"].is_number(), "the badge travels with the message");
    assert_eq!(send(&t, get("/push/badge").body(Body::empty()).unwrap()).await.status, StatusCode::UNAUTHORIZED);
    let b = send(&t, get("/push/badge").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(b.json()["count"].as_u64().unwrap() as usize, b.json()["items"].as_array().unwrap().len());
    assert!(b.json()["groups"].as_array().is_some_and(|g| !g.iter().any(|x| x == "test")), "a test notification never stays: {}", b.body);
    assert_eq!(send(&t, get("/push/message/nope").body(Body::empty()).unwrap()).await.status, StatusCode::NOT_FOUND);

    // The service worker.
    let r = send(&t, get("/sw.js").body(Body::empty()).unwrap()).await;
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/javascript"));
    assert!(r.body.starts_with(r#"const NAME = "Baby";"#) && r.body.contains("showNotification"));
}

#[tokio::test]
async fn entry_history_page() {
    let t = setup();
    let now = now();
    let id = t.app.db.with(|c| {
        let e = crate::entries::insert(c, 1, &crate::entries::NewEntry {
            at: now - 60, duration_s: None, text: "50 ml flesvoeding op 10 ml gekolfd".into(),
            fields: serde_json::from_value(json!({"bottle_formula_ml": 50, "pump_ml": 10})).unwrap(),
            attention_by: crate::entries::BY_AGENT, attention_why: Some("split".into()),
        }, now - 60).unwrap();
        let mut e2 = e.clone();
        e2.fields = serde_json::from_value(json!({"bottle_formula_ml": 40, "bottle_breastmilk_ml": 10})).unwrap();
        e2.attention_by = 0;
        e2.attention_why = None;
        crate::entries::save(c, 1, &e2, now).unwrap();
        Ok(e.entry_id)
    }).unwrap();
    let r = send(&t, get(&format!("/entry/{id}")).body(Body::empty()).unwrap()).await;
    assert!(r.location().starts_with("/login"), "needs a session");
    let r = send(&t, get(&format!("/entry/{id}")).header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("History · 2 versions"), "{}", r.body);
    assert!(r.body.contains(r#"<span class="chg"><span class="chip del" title="bottle_formula_ml: 50 ml">🍼 50 ml formula</span> → <span class="chip add" title="bottle_formula_ml: 40 ml">🍼 40 ml formula</span></span>"#), "{}", r.body);
    assert!(r.body.contains(r#"<span class="chip del" title="pump_ml: 10 ml">🥛 10 ml pumped</span>"#) && r.body.contains(r#"<span class="chip add" title="bottle_breastmilk_ml: 10 ml">🍼 10 ml breast milk</span>"#));
    assert!(r.body.contains(r#"<span class="chip add" title="bottle_formula_ml: 50 ml">"#), "v1 shows the fields it started with");
    assert!(r.body.contains("attention cleared (was: split)") && r.body.contains("🤖"));
    assert!(r.body.find(">v2<").unwrap() < r.body.find(">v1<").unwrap(), "newest first");
    let day = send(&t, get("/").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await;
    assert!(day.body.contains(&format!(r#"href="/entry/{id}""#)) && day.body.contains("✎ v2"));
    assert!(day.body.contains(r#"<div class="sub">Bas</div><div class="sub">✎ v2</div>"#), "who logged it, then the edit mark");
    assert_eq!(send(&t, get("/entry/999999").header(header::COOKIE, &t.cookie).body(Body::empty()).unwrap()).await.status, StatusCode::NOT_FOUND);
}
