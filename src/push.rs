//! Web Push (RFC 8030) with message encryption (RFC 8291) and VAPID (RFC 8292).
//!
//! Pushes carry no content: the encrypted payload is only a random message id.
//! The service worker fetches the text from `/push/message/{id}` over our own
//! TLS connection, so the push service (Apple, Google, Mozilla) relays nothing
//! but that id — and the classical (P-256) push encryption protects nothing
//! of lasting value.

use aes_gcm::aead::Aead;
use aes_gcm::Aes128Gcm;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use p256::ecdsa::signature::Signer;
use p256::{PublicKey, SecretKey};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::Sha256;

pub(crate) fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// A fresh random P-256 secret key.
pub fn random_secret() -> SecretKey {
    loop {
        let mut b = [0u8; 32];
        rand::fill(&mut b[..]);
        if let Ok(k) = SecretKey::from_slice(&b) {
            return k;
        }
    }
}

fn public_bytes(k: &SecretKey) -> Vec<u8> {
    use p256::elliptic_curve::sec1::ToSec1Point;
    k.public_key().to_sec1_point(false).as_bytes().to_vec()
}

/// The server's VAPID key pair (kept in `kv`, made on first use).
pub struct Vapid {
    key: SecretKey,
    /// Uncompressed public key, base64url: the page's `applicationServerKey`.
    pub public: String,
}

pub fn vapid(c: &Connection) -> rusqlite::Result<Vapid> {
    let stored: Option<Vec<u8>> = c.query_row("SELECT value FROM kv WHERE key = 'vapid_key'", [], |r| r.get(0)).optional()?;
    let key = match stored.and_then(|b| SecretKey::from_slice(&b).ok()) {
        Some(k) => k,
        None => {
            let k = random_secret();
            c.execute("INSERT OR IGNORE INTO kv (key, value) VALUES ('vapid_key', ?1)", [k.to_bytes().to_vec()])?;
            let b: Vec<u8> = c.query_row("SELECT value FROM kv WHERE key = 'vapid_key'", [], |r| r.get(0))?;
            SecretKey::from_slice(&b).map_err(|_| rusqlite::Error::InvalidQuery)?
        }
    };
    let public = B64.encode(public_bytes(&key));
    Ok(Vapid { key, public })
}

/// Encrypts `plaintext` for a subscription (RFC 8291, a single aes128gcm record).
/// `as_secret` and `salt` are fresh per message; they are parameters for tests.
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8], as_secret: &SecretKey, salt: [u8; 16]) -> Result<Vec<u8>, String> {
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "bad p256dh key".to_string())?;
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());
    let as_public = public_bytes(as_secret);
    // IKM = HKDF(auth, ecdh, "WebPush: info" 0 ua_public as_public, 32)
    let prk_key = hmac(auth, &[shared.raw_secret_bytes().as_slice()]);
    let ikm = hmac(&prk_key, &[b"WebPush: info\0", ua_public, &as_public, &[1]]);
    let prk = hmac(&salt, &[&ikm]);
    let cek = &hmac(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]])[..16];
    let nonce = &hmac(&prk, &[b"Content-Encoding: nonce\0", &[1]])[..12];
    let mut padded = plaintext.to_vec();
    padded.push(2); // last record, no padding
    let cipher = Aes128Gcm::new_from_slice(cek).map_err(|e| e.to_string())?;
    let nonce: &[u8; 12] = nonce.try_into().unwrap();
    let ct = cipher.encrypt(nonce.into(), padded.as_slice()).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(86 + ct.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// The VAPID `Authorization` header for a push endpoint.
pub fn vapid_header(v: &Vapid, endpoint: &str, sub: &str, now: i64) -> Result<String, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
    let aud = url.origin().ascii_serialization();
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(serde_json::json!({"aud": aud, "exp": now + 12 * 3600, "sub": sub}).to_string());
    let input = format!("{header}.{claims}");
    let sk = p256::ecdsa::SigningKey::from(&v.key);
    let sig: p256::ecdsa::Signature = sk.sign(input.as_bytes());
    Ok(format!("vapid t={input}.{}, k={}", B64.encode(sig.to_bytes()), v.public))
}

pub struct Subscription {
    pub id: i64,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub enum Outcome {
    Ok,
    /// The subscription is gone (404/410): forget it.
    Gone,
    Failed(String),
}

/// The push `Topic` for a notification group: at most 32 characters of
/// base64url (RFC 8030), and decodable as such (Apple's push service rejects
/// "attention", 9 characters, with BadWebPushTopic). A hash of the group:
/// 18 bytes, 24 characters.
pub fn topic(group: &str) -> String {
    use sha2::Digest;
    B64.encode(&Sha256::digest(group.as_bytes())[..18])
}

/// Sends one push carrying `payload` (encrypted) to a subscription.
#[allow(clippy::too_many_arguments)]
pub async fn send(client: &reqwest::Client, v: &Vapid, s: &Subscription, payload: &[u8], ttl: u32, topic: &str, sub: &str, now: i64) -> Outcome {
    let (Ok(p256dh), Ok(auth)) = (B64.decode(s.p256dh.trim_end_matches('=')), B64.decode(s.auth.trim_end_matches('='))) else {
        return Outcome::Gone;
    };
    let mut salt = [0u8; 16];
    rand::fill(&mut salt[..]);
    let body = match encrypt(&p256dh, &auth, payload, &random_secret(), salt) {
        Ok(b) => b,
        Err(_) => return Outcome::Gone,
    };
    let authz = match vapid_header(v, &s.endpoint, sub, now) {
        Ok(a) => a,
        Err(e) => return Outcome::Failed(e),
    };
    let r = client
        .post(&s.endpoint)
        .header("TTL", ttl.to_string())
        .header("Urgency", "high")
        .header("Topic", self::topic(topic))
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", authz)
        .body(body)
        .send()
        .await;
    match r {
        Ok(r) if r.status().is_success() => Outcome::Ok,
        Ok(r) if r.status().as_u16() == 404 || r.status().as_u16() == 410 => Outcome::Gone,
        Ok(r) => {
            let st = r.status();
            let text = r.text().await.unwrap_or_default();
            Outcome::Failed(format!("{st}: {}", text.chars().take(200).collect::<String>()))
        }
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Stores a message for a user and returns the id to push.
pub fn store_message(c: &Connection, user_id: i64, title: &str, body: &str, url: &str, tag: &str, now: i64) -> rusqlite::Result<String> {
    let id = crate::auth::random_token(16);
    c.execute("DELETE FROM push_message WHERE expires < ?1", [now])?;
    c.execute(
        "INSERT INTO push_message (hash, user_id, title, body, url, tag, expires) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![crate::auth::sha256_hex(&id), user_id, title, body, url, tag, now + 24 * 3600],
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        B64.decode(s.replace([' ', '\n'], "")).unwrap()
    }

    /// RFC 8291 Appendix A.
    #[test]
    fn rfc8291_vector() {
        let as_secret = SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        assert_eq!(public_bytes(&as_secret), d("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"));
        let ua_public = d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let plaintext = d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let out = encrypt(&ua_public, &auth, &plaintext, &as_secret, salt).unwrap();
        let mut want = d("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        want.extend(d("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"));
        assert_eq!(out, want);
    }

    #[test]
    fn topics_are_valid() {
        for g in ["feed", "attention", "changes", "vitamins", "test", "other"] {
            let t = topic(g);
            assert_eq!(t.len(), 24, "{g}");
            assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{g}: {t}");
            assert_eq!(B64.decode(&t).unwrap().len(), 18);
        }
        assert_ne!(topic("feed"), topic("attention"));
    }

    #[test]
    fn vapid_jwt_verifies() {
        use p256::ecdsa::signature::Verifier;
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value BLOB NOT NULL)").unwrap();
        let v = vapid(&c).unwrap();
        assert_eq!(vapid(&c).unwrap().public, v.public, "kept");
        let h = vapid_header(&v, "https://web.push.apple.com/abc", "https://example.org", 1000).unwrap();
        let t = h.strip_prefix("vapid t=").unwrap().split(", k=").next().unwrap();
        let (input, sig) = t.rsplit_once('.').unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&B64.decode(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["exp"], 1000 + 12 * 3600);
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&B64.decode(&v.public).unwrap()).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&B64.decode(sig).unwrap()).unwrap();
        vk.verify(input.as_bytes(), &sig).unwrap();
    }
}
