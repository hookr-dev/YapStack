// SPDX-License-Identifier: AGPL-3.0-only
//! The relay transport seam: `push` / `pull` / `completeness` against the blind
//! relay (`crates/yapstack-server`), speaking the `yapstack-common::sync` DTOs.
//!
//! The seam is a trait so the drain logic is testable against an in-memory relay
//! (`MockRelay`) that faithfully models the server's contract: a dense
//! commit-ordered `changeset_seq`, `(client_id, client_seq)` idempotency, and
//! per-client tail watermarks (A3). `HttpTransport` is the real reqwest client used
//! by the desktop runtime (wired in T010b).

use async_trait::async_trait;
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use uuid::Uuid;
use yapstack_common::sync::{
    ClientTail, CompletenessResponse, PullResponse, PulledChange, PushAck, PushRequest,
    PushResponse,
};

use crate::snapshot::SnapshotMeta;
use crate::SyncError;

#[async_trait]
pub trait SyncTransport: Send + Sync {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError>;
    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError>;
    async fn completeness(&self) -> Result<CompletenessResponse, SyncError>;

    /// R2: publish the encrypted DB snapshot (the seed device's bootstrap artifact).
    /// The relay stores it as an opaque blob and never reads it.
    async fn put_snapshot(&self, meta: SnapshotMeta, ciphertext: &[u8]) -> Result<(), SyncError>;

    /// R2: fetch the latest encrypted snapshot for the tenant, or `None` if the seed
    /// has not published one yet (a join device then falls back to full changeset pull).
    async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError>;
}

// ---- snapshot endpoint wire DTOs (client side) --------------------------------
//
// Defined here rather than in `yapstack-common` for now (T011 allowed-files scope); the
// field names mirror the server's `snapshot` handler exactly. T012 may hoist them into
// `yapstack-common` alongside the other sync DTOs (arch §14) — a mechanical move.

#[derive(Debug, Clone, Serialize)]
struct SnapshotPresignRequest {
    /// Lowercase hex SHA-256 of the CIPHERTEXT.
    sha256: String,
    size: u64,
    generation: u64,
    baseline_seq: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct SnapshotPresignResponse {
    already_exists: bool,
    upload_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SnapshotHeadResponse {
    present: bool,
    generation: u64,
    baseline_seq: i64,
    download_url: Option<String>,
}

/// Classify an HTTP status the drain must react to differently. A 401 becomes
/// [`SyncError::Unauthorized`] (the drain refreshes the token and retries once,
/// Bug A) instead of collapsing into a generic error via `error_for_status`.
/// Returns `Ok(resp)` for any non-401 outcome (2xx passes through; other non-2xx
/// is left for the caller's `error_for_status`). Kept a pure function of the
/// status so it is unit-testable without a live server.
fn classify_status(status: StatusCode) -> Option<SyncError> {
    if status == StatusCode::UNAUTHORIZED {
        Some(SyncError::Unauthorized)
    } else {
        None
    }
}

/// Classify a reqwest error raised by `send()`: a connection failure or a timeout means the
/// relay is UNREACHABLE (mirrors the T025 probe's `is_connect()`/`is_timeout()` split), so it
/// becomes the distinct [`SyncError::Network`] the drain maps to the amber "can't reach relay"
/// state. Every other reqwest error (body decode, or an HTTP status surfaced via
/// `error_for_status`) stays a generic [`SyncError::Http`]. The reqwest display carries the
/// relay URL only — never the bearer, which is sent as a header and is absent from the error.
fn map_send_error(e: reqwest::Error) -> SyncError {
    if e.is_connect() || e.is_timeout() {
        SyncError::Network(e.to_string())
    } else {
        SyncError::Http(e)
    }
}

/// Map a relay response to a distinct-401 result before decoding: a 401 is the
/// refreshable auth-expiry path, everything else keeps the existing
/// `error_for_status` behaviour.
fn check_auth(resp: Response) -> Result<Response, SyncError> {
    if let Some(e) = classify_status(resp.status()) {
        return Err(e);
    }
    Ok(resp.error_for_status()?)
}

// --------------------------------------------------------------- HTTP transport

/// Real relay client. `base_url` is the server root (e.g. `https://sync.example`);
/// the bearer is the access token from the auth flow (CRYPTO_SPEC §3). The bearer
/// is held behind a `RwLock` so the drain can swap in a rotated access token after
/// a refresh WITHOUT tearing down the transport (Bug A). The token is never logged.
pub struct HttpTransport {
    base_url: String,
    bearer: RwLock<String>,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: RwLock::new(bearer.into()),
            client: reqwest::Client::new(),
        }
    }

    /// Current access token, cloned for one request. Never logged.
    fn bearer(&self) -> String {
        self.bearer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the access token after a successful refresh (Bug A). Interior
    /// mutability so the shared `&HttpTransport` the drain holds can be updated
    /// between cycles. Never logs the token.
    pub fn set_bearer(&self, bearer: &str) {
        *self.bearer.write().unwrap_or_else(|e| e.into_inner()) = bearer.to_string();
    }
}

#[async_trait]
impl SyncTransport for HttpTransport {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError> {
        let r = check_auth(
            self.client
                .post(format!("{}/sync/push", self.base_url))
                .bearer_auth(self.bearer())
                .json(&req)
                .send()
                .await
                .map_err(map_send_error)?,
        )?;
        Ok(r.json().await?)
    }

    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
        let r = check_auth(
            self.client
                .get(format!("{}/sync/pull", self.base_url))
                .bearer_auth(self.bearer())
                .query(&[("since", since), ("limit", limit)])
                .send()
                .await
                .map_err(map_send_error)?,
        )?;
        Ok(r.json().await?)
    }

    async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
        let r = check_auth(
            self.client
                .get(format!("{}/sync/completeness", self.base_url))
                .bearer_auth(self.bearer())
                .send()
                .await
                .map_err(map_send_error)?,
        )?;
        Ok(r.json().await?)
    }

    async fn put_snapshot(&self, meta: SnapshotMeta, ciphertext: &[u8]) -> Result<(), SyncError> {
        // Presign step: content-address by ciphertext hash (§9-style; relay stays blind
        // and byte-free — the bytes go directly to object storage).
        let sha = crate::snapshot::ciphertext_sha256_hex(ciphertext);
        let presign: SnapshotPresignResponse = check_auth(
            self.client
                .post(format!("{}/snapshot/presign", self.base_url))
                .bearer_auth(self.bearer())
                .json(&SnapshotPresignRequest {
                    sha256: sha,
                    size: ciphertext.len() as u64,
                    generation: meta.generation,
                    baseline_seq: meta.baseline_seq,
                })
                .send()
                .await?,
        )?
        .json()
        .await?;
        if presign.already_exists {
            return Ok(());
        }
        let url = presign.upload_url.ok_or_else(|| {
            SyncError::Transport("snapshot presign returned no upload_url".into())
        })?;
        // Direct upload to object storage; content-length pinned by the presigned policy.
        self.client
            .put(url)
            .header("content-length", ciphertext.len())
            .body(ciphertext.to_vec())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
        let head: SnapshotHeadResponse = check_auth(
            self.client
                .get(format!("{}/snapshot", self.base_url))
                .bearer_auth(self.bearer())
                .send()
                .await?,
        )?
        .json()
        .await?;
        if !head.present {
            return Ok(None);
        }
        let url = head.download_url.ok_or_else(|| {
            SyncError::Transport("snapshot head present but no download_url".into())
        })?;
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        Ok(Some((
            SnapshotMeta {
                generation: head.generation,
                baseline_seq: head.baseline_seq,
            },
            bytes,
        )))
    }
}

// --------------------------------------------------------------- mock relay

struct StoredChange {
    changeset_seq: i64,
    client_id: Uuid,
    client_seq: i64,
    ciphertext: String,
    schema_version: i32,
    engine_version: i32,
}

/// In-memory relay faithfully modelling the server contract, for drain tests.
#[derive(Default)]
pub struct MockRelay {
    inner: Mutex<MockInner>,
}

#[derive(Default)]
struct MockInner {
    log: Vec<StoredChange>,
    // (client_id, client_seq) -> assigned changeset_seq (idempotency)
    seen: HashMap<(Uuid, i64), i64>,
    // latest published snapshot (meta, opaque ciphertext) — faithfully opaque: the
    // mock never inspects the bytes, mirroring the blind relay.
    snapshot: Option<(SnapshotMeta, Vec<u8>)>,
}

impl MockRelay {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SyncTransport for MockRelay {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError> {
        let mut g = self.inner.lock().unwrap();
        let mut acks = Vec::with_capacity(req.changes.len());
        for c in req.changes {
            let key = (c.client_id, c.client_seq);
            if let Some(&seq) = g.seen.get(&key) {
                acks.push(PushAck {
                    client_id: c.client_id,
                    client_seq: c.client_seq,
                    changeset_seq: seq,
                    deduplicated: true,
                });
                continue;
            }
            let seq = g.log.len() as i64 + 1; // dense, 1-based commit order
            g.log.push(StoredChange {
                changeset_seq: seq,
                client_id: c.client_id,
                client_seq: c.client_seq,
                ciphertext: c.ciphertext,
                schema_version: c.schema_version,
                engine_version: c.engine_version,
            });
            g.seen.insert(key, seq);
            acks.push(PushAck {
                client_id: c.client_id,
                client_seq: c.client_seq,
                changeset_seq: seq,
                deduplicated: false,
            });
        }
        let max = g.log.len() as i64;
        Ok(PushResponse {
            acks,
            max_changeset_seq: max,
        })
    }

    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
        let g = self.inner.lock().unwrap();
        let mut changes = Vec::new();
        let mut next = since;
        for c in g.log.iter().filter(|c| c.changeset_seq > since) {
            if changes.len() as i64 >= limit {
                break;
            }
            changes.push(PulledChange {
                changeset_seq: c.changeset_seq,
                client_id: c.client_id,
                client_seq: c.client_seq,
                ciphertext: c.ciphertext.clone(),
                schema_version: c.schema_version,
                engine_version: c.engine_version,
            });
            next = c.changeset_seq;
        }
        let has_more = g.log.iter().any(|c| c.changeset_seq > next);
        Ok(PullResponse {
            changes,
            next_seq: next,
            has_more,
        })
    }

    async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
        let g = self.inner.lock().unwrap();
        let mut per: HashMap<Uuid, i64> = HashMap::new();
        for c in &g.log {
            let e = per.entry(c.client_id).or_insert(0);
            *e = (*e).max(c.client_seq);
        }
        Ok(CompletenessResponse {
            max_changeset_seq: g.log.len() as i64,
            count: g.log.len() as i64,
            contiguous: true,
            per_client: per
                .into_iter()
                .map(|(client_id, max_client_seq)| ClientTail {
                    client_id,
                    max_client_seq,
                })
                .collect(),
        })
    }

    async fn put_snapshot(&self, meta: SnapshotMeta, ciphertext: &[u8]) -> Result<(), SyncError> {
        let mut g = self.inner.lock().unwrap();
        g.snapshot = Some((meta, ciphertext.to_vec()));
        Ok(())
    }

    async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
        let g = self.inner.lock().unwrap();
        Ok(g.snapshot.clone())
    }
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn classify_status_isolates_401() {
        // The pure classifier is the load-bearing distinction (Bug A): only 401 is the
        // refreshable auth-expiry path; every other status falls through to the caller's
        // `error_for_status`.
        assert!(matches!(
            classify_status(StatusCode::UNAUTHORIZED),
            Some(SyncError::Unauthorized)
        ));
        assert!(classify_status(StatusCode::OK).is_none());
        assert!(classify_status(StatusCode::INTERNAL_SERVER_ERROR).is_none());
        assert!(classify_status(StatusCode::PAYLOAD_TOO_LARGE).is_none());
        assert!(classify_status(StatusCode::FORBIDDEN).is_none());
    }

    /// End-to-end proof over a real socket that `push` maps a 401 to
    /// `SyncError::Unauthorized` while a 500 stays a generic (non-Unauthorized) error —
    /// the property the drain relies on to refresh-and-retry vs. warn-and-continue. A
    /// tiny raw HTTP responder (no extra deps) serves the two canned statuses.
    #[tokio::test(flavor = "current_thread")]
    async fn push_distinguishes_401_from_other_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for status_line in ["401 Unauthorized", "500 Internal Server Error"] {
                let (mut sock, _) = listener.accept().unwrap();
                // Drain the request until end-of-headers so the client's write completes
                // before we reply+close (the body here is a tiny empty PushRequest).
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = sock.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });

        let t = HttpTransport::new(format!("http://{addr}"), "access-token");
        let e401 = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            matches!(e401, SyncError::Unauthorized),
            "401 must map to Unauthorized, got {e401:?}"
        );
        let e500 = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            !matches!(e500, SyncError::Unauthorized),
            "500 must NOT map to Unauthorized, got {e500:?}"
        );

        // set_bearer swaps the token used on the next request without rebuilding.
        t.set_bearer("rotated-token");
        assert_eq!(t.bearer(), "rotated-token");

        server.join().unwrap();
    }

    /// A connection failure (nothing listening) must map to the typed
    /// [`SyncError::Network`] — the transport-layer "relay unreachable" signal the drain
    /// turns into the amber "can't reach relay" state — and NOT a generic
    /// [`SyncError::Http`]/`Unauthorized`. We bind a port, drop the listener so connects are
    /// refused, and prove `push` classifies the refused connect as `Network`.
    #[tokio::test(flavor = "current_thread")]
    async fn push_maps_connection_refused_to_network() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nothing is listening now → connect is refused

        let t = HttpTransport::new(format!("http://{addr}"), "access-token");
        let err = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            matches!(err, SyncError::Network(_)),
            "a refused connection must classify as Network (relay unreachable), got {err:?}"
        );
        assert!(err.is_network(), "SyncError::is_network() must agree");
    }
}
