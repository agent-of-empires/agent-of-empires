//! Web Push payload encryption and delivery.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::Serialize;
use sha2::Sha256;
use std::time::Duration;

use super::push::{base64_url_decode, PushState, Subscription};

/// Per-send HTTPS timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// TTL for the push notification, in seconds.
pub const PUSH_TTL_SECS: u32 = 60 * 60 * 24; // 24h

/// VAPID JWT lifetime. Spec allows up to 24h but 12h is common.
pub const VAPID_EXP_SECS: u64 = 60 * 60 * 12;

/// Outcome of one attempted push send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// 2xx from the push endpoint.
    Delivered,
    /// 410 Gone or 404 Not Found.
    Gone,
    /// Any other failure: timeout, connection error, 5xx, 429, etc.
    Failed,
}

#[derive(Serialize)]
struct VapidClaims {
    aud: String,
    exp: u64,
    sub: String,
}

/// The body shape the service worker expects from `event.data.json()`.
#[derive(Serialize)]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub url: String,
    pub tag: String,
    pub session_id: String,
}

/// Build a pre-configured reqwest client for push delivery.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(SEND_TIMEOUT)
        .build()
        .context("build reqwest client for push")
}

/// Construct the VAPID `Authorization: vapid t=<jwt>, k=<pub_b64url>` header for an endpoint.
pub fn vapid_auth_header(state: &PushState, endpoint: &str) -> Result<String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

    let aud = endpoint_origin(endpoint)?;
    let exp = chrono::Utc::now().timestamp() as u64 + VAPID_EXP_SECS;
    let claims = VapidClaims {
        aud,
        exp,
        sub: state.subject.clone(),
    };
    let header = Header::new(Algorithm::ES256);
    let key = EncodingKey::from_ec_pem(state.vapid.private_pem.as_bytes())
        .context("load VAPID private key for JWT signing")?;
    let jwt = encode(&header, &claims, &key).context("sign VAPID JWT")?;
    Ok(format!("vapid t={}, k={}", jwt, state.vapid.public_b64url))
}

fn endpoint_origin(endpoint: &str) -> Result<String> {
    let url = reqwest::Url::parse(endpoint).context("parse push endpoint URL")?;
    let scheme = url.scheme();
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("push endpoint has no host"))?;
    Ok(match url.port() {
        Some(p) => format!("{}://{}:{}", scheme, host, p),
        None => format!("{}://{}", scheme, host),
    })
}

/// Encrypt `plaintext` into an aes128gcm-encoded body per RFC 8188/8291 targeting the given
/// subscription.
pub fn encrypt_aes128gcm(subscription: &Subscription, plaintext: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes128Gcm, KeyInit};
    use hkdf::Hkdf;
    use p256::ecdh::diffie_hellman;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::{PublicKey, SecretKey};

    // Subscription-side key material.
    let p_ua_bytes =
        base64_url_decode(&subscription.p256dh).context("decode subscription p256dh")?;
    let auth_secret = base64_url_decode(&subscription.auth).context("decode subscription auth")?;
    if auth_secret.len() != 16 {
        return Err(anyhow!(
            "subscription auth must be 16 bytes, got {}",
            auth_secret.len()
        ));
    }
    let p_ua = PublicKey::from_sec1_bytes(&p_ua_bytes)
        .context("parse subscription p256dh as SEC1 P-256")?;

    // Ephemeral server keypair (fresh per push).
    let mut eph_seed = [0u8; 32];
    getrandom::fill(&mut eph_seed).map_err(|e| anyhow!("getrandom for ephemeral key: {}", e))?;
    let d_as = SecretKey::from_slice(&eph_seed).context("derive ephemeral P-256 key from seed")?;
    let p_as = d_as.public_key();
    let p_as_encoded = p_as.to_encoded_point(false);
    let p_as_bytes = p_as_encoded.as_bytes();
    if p_as_bytes.len() != 65 {
        return Err(anyhow!(
            "ephemeral public key unexpected length {}",
            p_as_bytes.len()
        ));
    }

    // ECDH shared secret.
    let shared = diffie_hellman(d_as.to_nonzero_scalar(), p_ua.as_affine());
    let shared_bytes = shared.raw_secret_bytes();

    // First HKDF.
    let mut info1 = Vec::with_capacity(14 + 65 + 65);
    info1.extend_from_slice(b"WebPush: info\0");
    info1.extend_from_slice(&p_ua_bytes);
    info1.extend_from_slice(p_as_bytes);

    let hk1 = Hkdf::<Sha256>::new(Some(&auth_secret), shared_bytes.as_ref());
    let mut ikm = [0u8; 32];
    hk1.expand(&info1, &mut ikm)
        .map_err(|e| anyhow!("HKDF expand for IKM: {}", e))?;

    // Random salt.
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow!("getrandom for salt: {}", e))?;

    // Second HKDF: derive CEK and nonce from (salt, IKM).
    let hk2 = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    hk2.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|e| anyhow!("HKDF expand for CEK: {}", e))?;
    let mut nonce = [0u8; 12];
    hk2.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|e| anyhow!("HKDF expand for nonce: {}", e))?;

    // AES-128-GCM encryption.
    let cipher = Aes128Gcm::new((&cek).into());
    let mut plaintext_padded = plaintext.to_vec();
    plaintext_padded.push(0x02);
    let ciphertext = cipher
        .encrypt((&nonce).into(), plaintext_padded.as_ref())
        .map_err(|e| anyhow!("AES-GCM encrypt: {}", e))?;

    // aes128gcm body layout.
    let record_size: u32 = (ciphertext.len() + 17)
        .max(18)
        .try_into()
        .unwrap_or(u32::MAX);
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&record_size.to_be_bytes());
    body.push(65u8);
    body.extend_from_slice(p_as_bytes);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Send a single push notification.
pub async fn send_one(
    client: &reqwest::Client,
    state: &PushState,
    subscription: &Subscription,
    payload: &PushPayload,
) -> SendOutcome {
    match send_one_inner(client, state, subscription, payload).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(target: "http.middleware",
                endpoint = %subscription.endpoint,
                error = %e,
                "push: send_one error, marking Failed"
            );
            SendOutcome::Failed
        }
    }
}

async fn send_one_inner(
    client: &reqwest::Client,
    state: &PushState,
    subscription: &Subscription,
    payload: &PushPayload,
) -> Result<SendOutcome> {
    let plaintext = serde_json::to_vec(payload).context("serialize push payload")?;
    let encrypted = encrypt_aes128gcm(subscription, &plaintext)?;
    let authorization = vapid_auth_header(state, &subscription.endpoint)?;

    let resp = client
        .post(&subscription.endpoint)
        .header("Authorization", authorization)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Encoding", "aes128gcm")
        .header("TTL", PUSH_TTL_SECS.to_string())
        .body(encrypted)
        .send()
        .await
        .context("POST to push endpoint")?;

    let status = resp.status();
    if status.is_success() {
        Ok(SendOutcome::Delivered)
    } else if status == reqwest::StatusCode::GONE || status == reqwest::StatusCode::NOT_FOUND {
        Ok(SendOutcome::Gone)
    } else {
        let text = resp
            .text()
            .await
            .unwrap_or_else(|_| String::from("(body unreadable)"));
        tracing::warn!(target: "http.middleware",
            endpoint = %subscription.endpoint,
            status = %status,
            body = %text,
            "push: non-success response"
        );
        Ok(SendOutcome::Failed)
    }
}

/// URL-safe base64 without padding, for things that aren't subscription keys.
pub fn b64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_origin_strips_path_and_query() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/abc?x=1").unwrap(),
            "https://fcm.googleapis.com"
        );
        assert_eq!(
            endpoint_origin("https://updates.push.services.mozilla.com/wpush/v2/XYZ").unwrap(),
            "https://updates.push.services.mozilla.com"
        );
        assert_eq!(
            endpoint_origin("http://localhost:8080/push/x").unwrap(),
            "http://localhost:8080"
        );
    }

    #[test]
    fn endpoint_origin_rejects_garbage() {
        assert!(endpoint_origin("not-a-url").is_err());
        assert!(endpoint_origin("data:text/plain,hi").is_err());
    }

    #[test]
    fn encrypt_aes128gcm_shape_and_header() {
        // Subscription key material from a real browser push subscription is base64url.
        let p_ua_bytes = {
            // Generate a valid P-256 public key.
            let seed = [7u8; 32];
            let sk = p256::SecretKey::from_slice(&seed).unwrap();
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            let pt = sk.public_key().to_encoded_point(false);
            pt.as_bytes().to_vec()
        };
        let subscription = super::super::push::Subscription {
            endpoint: "https://example.com/push/xyz".to_string(),
            p256dh: b64url(&p_ua_bytes),
            auth: b64url(&[9u8; 16]),
            owner_token_hash: [0u8; 32],
            user_agent: String::new(),
            created_at: chrono::Utc::now(),
            generation: 0,
            origin: "http://localhost:8080".to_string(),
        };

        let body = encrypt_aes128gcm(&subscription, b"hello").expect("encrypt");
        // Layout: salt(16) + record_size(4) + idlen(1) + keyid(65) + ciphertext(>=5+1+16)
        assert!(body.len() >= 16 + 4 + 1 + 65 + 5 + 1 + 16);
        assert_eq!(body[20], 65, "idlen byte must be 65 for uncompressed P-256");
    }
}
