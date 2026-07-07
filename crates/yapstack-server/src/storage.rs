// SPDX-License-Identifier: AGPL-3.0-only
//! S3/MinIO SigV4 **presigning** (architecture §9). This is pure local HMAC — the relay
//! computes a signed URL and hands it to the client; it NEVER uploads, downloads,
//! re-hashes, or otherwise touches the bytes (ZERO outbound calls). The client PUTs the
//! ciphertext directly to object storage, which verifies the pinned content-length.
//!
//! Presigned URLs are BEARER CAPABILITIES: callers must never log them.

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
    fn get_presign_signs_only_host() {
        let key = object_key(uuid::Uuid::nil(), "abcd");
        let g = presign(&cfg(), "GET", &key, None, ts());
        assert!(g.url.contains("X-Amz-SignedHeaders=host"));
        assert!(!g.url.contains("content-length"));
    }
}
