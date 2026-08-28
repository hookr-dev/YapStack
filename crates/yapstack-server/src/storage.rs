// SPDX-License-Identifier: AGPL-3.0-only
//! S3/MinIO SigV4 **presigning** (architecture §9). This is pure local HMAC — the relay
//! computes a signed URL and hands it to the client; it NEVER uploads, downloads,
//! re-hashes, or otherwise touches blob bytes. The client PUTs the ciphertext directly to
//! object storage, which verifies the pinned content-length.
//!
//! The single exception is the **metadata-only HEAD** in [`object_exists`] (audio D8): it
//! reads only object existence/metadata (never bytes, never plaintext), gating the
//! `already_exists` vs. fresh-`upload_url` decision so a dead PUT can never be mistaken for
//! a stored blob. This is the only outbound call the relay makes.
//!
//! Presigned URLs are BEARER CAPABILITIES: callers must never log them.

use std::sync::OnceLock;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::config::StorageConfig;

type HmacSha256 = Hmac<Sha256>;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";
const AWS4_REQUEST: &str = "aws4_request";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Build the object key for a tenant's ciphertext blob (architecture §9):
/// `tenants/{workspace_id}/blobs/{ciphertext_sha256_hex}`.
#[must_use]
pub fn object_key(workspace_id: uuid::Uuid, ciphertext_sha256_hex: &str) -> String {
    format!("tenants/{workspace_id}/blobs/{ciphertext_sha256_hex}")
}

/// Build the object key for a tenant's R2 snapshot ciphertext:
/// `tenants/{workspace_id}/snapshot/{ciphertext_sha256_hex}`. Deliberately its own named
/// function rather than a `kind` argument on [`object_key`]: these keys are the addresses a
/// presigned DELETE is issued against, so the segment must not be typo-able at a call site.
#[must_use]
pub fn snapshot_object_key(workspace_id: uuid::Uuid, ciphertext_sha256_hex: &str) -> String {
    format!("tenants/{workspace_id}/snapshot/{ciphertext_sha256_hex}")
}

/// Parse a client-supplied ciphertext SHA-256 into `(bytes, lowercase_hex)`. Shared by the
/// audio and snapshot presign paths — both address blobs by their ciphertext hash.
///
/// # Errors
/// [`AppError::BadRequest`] if the input is not 64 hex characters.
pub(crate) fn parse_sha256(s: &str) -> Result<(Vec<u8>, String), crate::error::AppError> {
    use crate::error::AppError;
    let lower = s.to_ascii_lowercase();
    if lower.len() != 64 || !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest("sha256: expected 64 hex chars".into()));
    }
    let bytes =
        hex::decode(&lower).map_err(|_| AppError::BadRequest("sha256: invalid hex".into()))?;
    Ok((bytes, lower))
}

/// RFC 3986 percent-encoding. When `encode_slash` is false, `/` is preserved (path
/// segments); when true, everything but the unreserved set is encoded (query values).
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (!encode_slash && b == b'/');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn canonical_path(bucket: &str, key: &str) -> String {
    let mut p = String::from("/");
    p.push_str(&uri_encode(bucket, true));
    for seg in key.split('/') {
        p.push('/');
        p.push_str(&uri_encode(seg, true));
    }
    p
}

fn signing_key(secret: &str, datestamp: &str, region: &str) -> Vec<u8> {
    fn mac(key: &[u8], msg: &str) -> Vec<u8> {
        let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
        m.update(msg.as_bytes());
        m.finalize().into_bytes().to_vec()
    }
    let k_date = mac(format!("AWS4{secret}").as_bytes(), datestamp);
    let k_region = mac(&k_date, region);
    let k_service = mac(&k_region, SERVICE);
    mac(&k_service, AWS4_REQUEST)
}

/// A single presigned URL result. The URL is a bearer capability — do not log it.
#[derive(Debug, Clone)]
pub struct PresignedUrl {
    pub url: String,
}

/// Presign a request against object storage. `method` is `PUT` or `GET`. When
/// `content_length` is `Some`, it is pinned via a signed `content-length` header, so a
/// client that declares one size and uploads another is rejected by object storage —
/// the declared size is enforced by the policy, never trusted by the relay.
///
/// `now` is injected for determinism/testability (UTC).
#[must_use]
pub fn presign(
    cfg: &StorageConfig,
    method: &str,
    key: &str,
    content_length: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
) -> PresignedUrl {
    let endpoint = cfg.public_endpoint.as_deref().unwrap_or(&cfg.endpoint);
    let endpoint = endpoint.trim_end_matches('/');
    // Host for the signed `host` header = the authority the client will connect to.
    let host = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);

    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let datestamp = now.format("%Y%m%d").to_string();
    let scope = format!("{datestamp}/{}/{SERVICE}/{AWS4_REQUEST}", cfg.region);

    let path = canonical_path(&cfg.bucket, key);

    // Signed headers: host always; content-length when pinning an upload size.
    let mut signed_header_pairs: Vec<(String, String)> =
        vec![("host".to_string(), host.to_string())];
    if let Some(len) = content_length {
        signed_header_pairs.push(("content-length".to_string(), len.to_string()));
    }
    signed_header_pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_headers = signed_header_pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = signed_header_pairs
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();

    let credential = format!("{}/{scope}", cfg.access_key_id);
    // Query params must be sorted by key; values URI-encoded.
    let mut query: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".to_string(), ALGORITHM.to_string()),
        ("X-Amz-Credential".to_string(), credential),
        ("X-Amz-Date".to_string(), amz_date.clone()),
        (
            "X-Amz-Expires".to_string(),
            cfg.presign_ttl_secs.to_string(),
        ),
        ("X-Amz-SignedHeaders".to_string(), signed_headers.clone()),
    ];
    query.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
        .collect::<Vec<_>>()
        .join("&");

    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{UNSIGNED_PAYLOAD}"
    );
    let hashed_request = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("{ALGORITHM}\n{amz_date}\n{scope}\n{hashed_request}");

    let key_bytes = signing_key(&cfg.secret_access_key, &datestamp, &cfg.region);
    let mut mac = HmacSha256::new_from_slice(&key_bytes).expect("hmac accepts any key length");
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let url = format!("{endpoint}{path}?{canonical_query}&X-Amz-Signature={signature}");
    PresignedUrl { url }
}

/// The one outbound HTTP client, built lazily and shared: a per-call `Client` rebuilds the
/// connection pool and TLS config on every presign dedup hit and every GC'd blob. The build
/// error is STORED rather than panicked on — the GC sweep runs in a background task, so a
/// failure must surface as [`AppError::Unavailable`] on the call, never as an abort.
static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

fn storage_client() -> Result<&'static reqwest::Client, crate::error::AppError> {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| crate::error::AppError::Unavailable(format!("storage client: {e}")))
}

/// Presign `method` for `key`, issue it, and return only the status code. The response body
/// is never read — no blob bytes cross the relay. The presigned URL is a bearer capability:
/// it must never be logged, including in the error paths below.
async fn signed_request(
    cfg: &StorageConfig,
    method: reqwest::Method,
    key: &str,
) -> Result<u16, crate::error::AppError> {
    use crate::error::AppError;
    let signed = presign(cfg, method.as_str(), key, None, chrono::Utc::now());
    let resp = storage_client()?
        .request(method.clone(), &signed.url)
        .send()
        .await
        .map_err(|e| AppError::Unavailable(format!("storage {method} failed: {e}")))?;
    Ok(resp.status().as_u16())
}

/// Metadata-only existence check against object storage (audio D8). Presigns a `HEAD` and
/// issues it: `2xx` → present, `404` → absent. It reads ONLY existence/metadata — never
/// blob bytes, never plaintext. A short connect/read timeout keeps a slow or unreachable
/// backend from stalling a presign; the caller treats any error as "not confirmed present"
/// and errs toward re-upload (never a phantom `done`).
///
/// # Errors
/// Returns [`AppError::Unavailable`] if the HTTP client cannot be built, the request
/// fails, or the backend returns an unexpected status (neither 2xx nor 404).
pub async fn object_exists(cfg: &StorageConfig, key: &str) -> Result<bool, crate::error::AppError> {
    use crate::error::AppError;
    match signed_request(cfg, reqwest::Method::HEAD, key).await? {
        200..=299 => Ok(true),
        404 => Ok(false),
        other => Err(AppError::Unavailable(format!(
            "storage HEAD returned unexpected status {other}"
        ))),
    }
}

/// Delete an object from storage (relay blob GC). Presigns a `DELETE` and issues it. S3/MinIO
/// DELETE is IDEMPOTENT: a missing object still returns `204 No Content`, and some backends
/// answer `404` — both mean "not present after this call", so both are treated as success.
/// This is the ONLY mutating outbound call the relay makes, and it moves no blob bytes: it
/// signs a delete and reads only the status line. The presigned URL is a bearer capability —
/// the caller must never log it.
///
/// Ordering contract (see `gc.rs`): the caller deletes the OBJECT first, then the
/// `audio_blobs` row. A crash between the two leaves a row without an object, which the next
/// sweep re-deletes safely; the reverse order would orphan storage bytes forever.
///
/// # Errors
/// Returns [`AppError::Unavailable`] if the HTTP client cannot be built, the request fails,
/// or the backend returns a status that is neither 2xx nor 404 (so the caller keeps the row
/// and retries on the next sweep rather than dropping a still-referenced blob).
pub async fn delete_object(cfg: &StorageConfig, key: &str) -> Result<(), crate::error::AppError> {
    use crate::error::AppError;
    match signed_request(cfg, reqwest::Method::DELETE, key).await? {
        200..=299 | 404 => Ok(()),
        other => Err(AppError::Unavailable(format!(
            "storage DELETE returned unexpected status {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StorageConfig {
        StorageConfig {
            endpoint: "http://minio:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "yapstack".to_string(),
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            public_endpoint: None,
            presign_ttl_secs: 900,
        }
    }

    fn ts() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap()
    }

    #[test]
    fn presign_is_deterministic_and_well_formed() {
        let key = object_key(uuid::Uuid::nil(), "abcd");
        let a = presign(&cfg(), "PUT", &key, Some(1024), ts());
        let b = presign(&cfg(), "PUT", &key, Some(1024), ts());
        assert_eq!(a.url, b.url, "same inputs must yield the same signature");
        assert!(a.url.contains("X-Amz-Signature="));
        assert!(a.url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        // content-length pinned => it is in the signed headers.
        assert!(a.url.contains("content-length%3Bhost"));
        assert!(a.url.starts_with("http://minio:9000/yapstack/tenants/"));
    }

    #[test]
    fn pinning_content_length_changes_the_signature() {
        let key = object_key(uuid::Uuid::nil(), "abcd");
        let pinned = presign(&cfg(), "PUT", &key, Some(1024), ts());
        let other = presign(&cfg(), "PUT", &key, Some(2048), ts());
        assert_ne!(
            pinned.url, other.url,
            "a different declared size must produce a different signature"
        );
    }

    #[test]
    fn delete_presign_signs_only_host_and_is_deterministic() {
        let key = object_key(uuid::Uuid::nil(), "abcd");
        let a = presign(&cfg(), "DELETE", &key, None, ts());
        let b = presign(&cfg(), "DELETE", &key, None, ts());
        assert_eq!(a.url, b.url, "same inputs must yield the same signature");
        assert!(a.url.contains("X-Amz-SignedHeaders=host"));
        assert!(!a.url.contains("content-length"));
        assert!(a.url.starts_with("http://minio:9000/yapstack/tenants/"));
    }

    #[test]
    fn get_presign_signs_only_host() {
        let key = object_key(uuid::Uuid::nil(), "abcd");
        let g = presign(&cfg(), "GET", &key, None, ts());
        assert!(g.url.contains("X-Amz-SignedHeaders=host"));
        assert!(!g.url.contains("content-length"));
    }
}
