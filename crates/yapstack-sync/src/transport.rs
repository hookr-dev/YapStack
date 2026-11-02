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
    CapabilityBlocker, ClientTail, CompletenessResponse, PresignResponse, PullResponse,
    PulledChange, PushAck, PushRequest, PushResponse, RaiseSyncCapabilityRequest,
    SyncCapabilityBlocked, SyncCapabilityLevels, SyncCapabilityRefusal, BATCH_CAPABILITY_HEADER,
    SNAPSHOT_CAPABILITY_HEADER, SYNC_CAPABILITY_HEADER,
};

use crate::snapshot::SnapshotMeta;
use crate::{SyncError, SYNC_CAPABILITY_LEVEL};

// async_trait copies `Result`'s `#[must_use]` onto the boxed futures it
// generates, which clippy ≥1.99 flags as double_must_use in macro output.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait SyncTransport: Send + Sync {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError>;
    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError>;
    async fn completeness(&self) -> Result<CompletenessResponse, SyncError>;

    /// Push a batch whose entries all need sync capability `batch_capability` (ADR 0006
    /// §5.4). The relay refuses it with [`SyncError::CapabilityFloor`] when its floor is
    /// lower. Level 0 is a plain [`Self::push`].
    ///
    /// Default: level 0 delegates to `push`; anything higher is refused as if the relay's
    /// floor were 0 (test transports that never model the gate).
    async fn push_batch(
        &self,
        req: PushRequest,
        batch_capability: i32,
    ) -> Result<PushResponse, SyncError> {
        if batch_capability > 0 {
            return Err(SyncError::CapabilityFloor {
                batch: batch_capability,
                required: 0,
                requested: 0,
            });
        }
        self.push(req).await
    }

    /// Ask the relay to raise the workspace floor to at least `level` (`PUT
    /// /sync/capability`). The relay re-checks every recently seen device itself and
    /// answers [`SyncError::CapabilityBlocked`] naming the ones below `level`.
    ///
    /// Default: unsupported.
    async fn raise_sync_capability(&self, _level: i32) -> Result<SyncCapabilityLevels, SyncError> {
        Err(SyncError::Transport(
            "sync capability raise not supported".into(),
        ))
    }

    /// R2: publish the encrypted DB snapshot (the seed device's bootstrap artifact).
    /// The relay stores it as an opaque blob and never reads it. `content_capability` is the
    /// highest sync capability level among the tables the snapshot holds rows for
    /// ([`crate::schema::snapshot_content_capability`], ADR 0006 §5.5); the relay refuses it
    /// with [`SyncError::CapabilityFloor`] when its floor is lower.
    ///
    /// Default: unsupported (test transports that never touch the snapshot seam inherit this).
    async fn put_snapshot(
        &self,
        _meta: SnapshotMeta,
        _ciphertext: &[u8],
        _content_capability: i32,
    ) -> Result<(), SyncError> {
        Err(SyncError::Transport("snapshot put not supported".into()))
    }

    /// R2: fetch the latest encrypted snapshot for the tenant, or `None` if the seed
    /// has not published one yet (a join device then falls back to full changeset pull).
    ///
    /// Default: unsupported — NOT `Ok(None)`. `Ok(None)` is reserved for a real transport's
    /// "no snapshot published yet" answer, so a transport that never implemented this cannot
    /// silently degrade a join into a lossy full-changeset pull (it fails as a transport fault).
    async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
        Err(SyncError::Transport("snapshot get not supported".into()))
    }

    /// Audio round-trip: presign a blob upload addressed by the `session_audio_parts` row
    /// UUID (`part_id`, D6). The server's existence-checked response (D8) either reports
    /// `already_exists` (object present — nothing to upload) or returns an `upload_url`.
    /// `sha256` is the lowercase hex of the WHOLE encrypted `audio_blob`.
    ///
    /// Default: unsupported (test transports that never touch audio inherit this).
    async fn presign_audio(
        &self,
        _sha256: &str,
        _size: u64,
        _part_id: &str,
        _session_id: &str,
    ) -> Result<PresignResponse, SyncError> {
        Err(SyncError::Transport("audio presign not supported".into()))
    }

    /// PUT the encrypted temp file at `temp_path` to the presigned `upload_url` with a
    /// STREAMING body (O(chunk) memory — never a buffered `vec`), content-length pinned.
    ///
    /// Default: unsupported.
    async fn put_audio(
        &self,
        _upload_url: &str,
        _temp_path: &std::path::Path,
        _content_length: u64,
    ) -> Result<(), SyncError> {
        Err(SyncError::Transport("audio put not supported".into()))
    }

    /// Fetch the blob for `part_id` to `dest_path` (S3 on-demand playback), reporting live
    /// download progress into `progress` (`received` bytes and the server-declared `total`
    /// content-length) and honouring cooperative cancellation (`progress.cancel`).
    /// `Ok(false)` = blob not yet on the relay (HTTP 404 — the row converged before its audio
    /// uploaded), `Ok(true)` = completed download. A caller-requested cancel aborts the
    /// stream, removes the partial file, and returns [`SyncError::Transport`] carrying
    /// [`FETCH_CANCELLED`] so the fetch layer can classify it as a user cancel (not an
    /// error) WITHOUT a new shared-error variant. The body is streamed frame-by-frame —
    /// never buffered whole (O(frame) memory, matching the upload path's discipline).
    ///
    /// Default: unsupported.
    async fn get_audio_streaming(
        &self,
        _part_id: &str,
        _dest_path: &std::path::Path,
        _progress: &crate::audio::FetchProgress,
    ) -> Result<bool, SyncError> {
        Err(SyncError::Transport("audio get not supported".into()))
    }
}

/// Cancellation sentinel carried in a [`SyncError::Transport`] when a streaming fetch is
/// aborted via [`crate::audio::FetchProgress::request_cancel`]. Kept a string marker rather
/// than a new [`SyncError`] variant so the fetch layer can classify a user cancel distinctly
/// without touching the shared error enum.
pub const FETCH_CANCELLED: &str = "fetch cancelled by caller";

// ---- snapshot endpoint wire DTOs (client side) --------------------------------
//
// Client-side mirror of the relay's snapshot handler DTOs (the compaction seam, SYNC_SPEC
// §15.F); the field names match `crates/yapstack-server/src/snapshot.rs` exactly. Hoisting
// them into `yapstack-common` is a mechanical move for the compaction slice.

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
/// `error_for_status`) stays a generic [`SyncError::Http`]. The URL is stripped from the
/// error (`without_url`): the audio PUT targets a presigned object-storage URL whose query
/// string is a bearer capability, and this string reaches the log file and the UI. The
/// bearer token is a header and never part of a reqwest error.
fn map_send_error(e: reqwest::Error) -> SyncError {
    let e = e.without_url();
    if e.is_connect() || e.is_timeout() {
        SyncError::Network(e.to_string())
    } else {
        SyncError::Http(e)
    }
}

/// True if `err` is the relay's positively identified 409 `conflict` (see
/// [`check_capability`]) — the G3 signal that the relay already holds this
/// `(client_id, client_seq)` under DIFFERENT ciphertext, so this client's counter regressed
/// (a DB restore) and re-minted a seq the relay already passed. The drain matches on it to
/// run the reseed-and-recover path, which drops the unacked outbox, so an unreadable or
/// unrecognised 409 must never match. Kept a pure predicate so it is unit-testable.
pub(crate) fn is_conflict(err: &SyncError) -> bool {
    matches!(err, SyncError::RelayConflict(_))
}

/// Classify the relay's 409 and 426 answers by their body `code` (ADR 0006 §5): 426
/// `update_required`, 409 `sync_capability_floor` and 409 `sync_capability_blocked` become
/// their typed errors, and a 409 `conflict` becomes [`SyncError::RelayConflict`], the only
/// error [`is_conflict`] recognises. Nothing is guessed: a body that can't be read, isn't
/// JSON, carries an unknown code, or a known code with missing fields stays an error that
/// names the status, and never reaches the reseed path that drops the unacked outbox.
/// `level_sent` is the capability this request declared. Every other status passes
/// through untouched.
async fn check_capability(resp: Response, level_sent: i32) -> Result<Response, SyncError> {
    let status = resp.status();
    if status != StatusCode::UPGRADE_REQUIRED && status != StatusCode::CONFLICT {
        return Ok(resp);
    }
    let body = resp.bytes().await.map_err(map_send_error)?;
    let unrecognised = |what: &str| {
        SyncError::Transport(format!(
            "relay answered {status} with {what}; nothing was changed locally"
        ))
    };
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| unrecognised("a body that isn't JSON"))?;
    let code = value
        .get("code")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    let refusal = || {
        serde_json::from_value::<SyncCapabilityRefusal>(value.clone())
            .map_err(|_| unrecognised(&format!("an incomplete `{code}` body")))
    };
    match (status, code.as_str()) {
        (StatusCode::UPGRADE_REQUIRED, "update_required") => {
            let r = refusal()?;
            Err(SyncError::UpdateRequired {
                level: level_sent,
                required: r.required_sync_capability,
                requested: r.requested_sync_capability,
            })
        }
        (StatusCode::CONFLICT, "sync_capability_floor") => {
            let r = refusal()?;
            Err(SyncError::CapabilityFloor {
                batch: level_sent,
                required: r.required_sync_capability,
                requested: r.requested_sync_capability,
            })
        }
        (StatusCode::CONFLICT, "sync_capability_blocked") => {
            let b = serde_json::from_value::<SyncCapabilityBlocked>(value.clone())
                .map_err(|_| unrecognised("an incomplete `sync_capability_blocked` body"))?;
            Err(SyncError::CapabilityBlocked {
                level: level_sent,
                blockers: b.blockers,
            })
        }
        (StatusCode::CONFLICT, "conflict") => Err(SyncError::RelayConflict(
            value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
        )),
        _ => Err(unrecognised(&format!("an unrecognised code `{code}`"))),
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

    /// The transport's HTTP client (a cheap handle onto one shared connection pool), so a
    /// lane's side requests reuse it instead of building a client per call.
    pub fn http_client(&self) -> reqwest::Client {
        self.client.clone()
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
        self.push_batch(req, 0).await
    }

    async fn push_batch(
        &self,
        req: PushRequest,
        batch_capability: i32,
    ) -> Result<PushResponse, SyncError> {
        let resp = self
            .client
            .post(format!("{}/sync/push", self.base_url))
            .bearer_auth(self.bearer())
            .header(BATCH_CAPABILITY_HEADER, batch_capability.to_string())
            .json(&req)
            .send()
            .await
            .map_err(map_send_error)?;
        let r = check_auth(check_capability(resp, batch_capability).await?)?;
        Ok(r.json().await?)
    }

    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
        let resp = self
            .client
            .get(format!("{}/sync/pull", self.base_url))
            .bearer_auth(self.bearer())
            .header(SYNC_CAPABILITY_HEADER, SYNC_CAPABILITY_LEVEL.to_string())
            .query(&[("since", since), ("limit", limit)])
            .send()
            .await
            .map_err(map_send_error)?;
        let r = check_auth(check_capability(resp, SYNC_CAPABILITY_LEVEL).await?)?;
        Ok(r.json().await?)
    }

    async fn raise_sync_capability(&self, level: i32) -> Result<SyncCapabilityLevels, SyncError> {
        let resp = self
            .client
            .put(format!("{}/sync/capability", self.base_url))
            .bearer_auth(self.bearer())
            .header(SYNC_CAPABILITY_HEADER, SYNC_CAPABILITY_LEVEL.to_string())
            .json(&RaiseSyncCapabilityRequest {
                required_sync_capability: level,
            })
            .send()
            .await
            .map_err(map_send_error)?;
        let r = check_auth(check_capability(resp, level).await?)?;
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

    // ADR 0006 §5.5: the relay refuses a snapshot whose declared content level
    // (`X-YapStack-Snapshot-Capability`) is above its floor.
    async fn put_snapshot(
        &self,
        meta: SnapshotMeta,
        ciphertext: &[u8],
        content_capability: i32,
    ) -> Result<(), SyncError> {
        // Presign step: content-address by ciphertext hash (§9-style; relay stays blind
        // and byte-free — the bytes go directly to object storage).
        let sha = crate::snapshot::ciphertext_sha256_hex(ciphertext);
        let resp = self
            .client
            .post(format!("{}/snapshot/presign", self.base_url))
            .bearer_auth(self.bearer())
            .header(SYNC_CAPABILITY_HEADER, SYNC_CAPABILITY_LEVEL.to_string())
            .header(SNAPSHOT_CAPABILITY_HEADER, content_capability.to_string())
            .json(&SnapshotPresignRequest {
                sha256: sha,
                size: ciphertext.len() as u64,
                generation: meta.generation,
                baseline_seq: meta.baseline_seq,
            })
            .send()
            .await?;
        // A 426 refers to this build's level, a 409 floor refusal to the content's.
        let resp = match check_capability(resp, SYNC_CAPABILITY_LEVEL).await {
            Err(SyncError::CapabilityFloor {
                required,
                requested,
                ..
            }) => {
                return Err(SyncError::CapabilityFloor {
                    batch: content_capability,
                    required,
                    requested,
                })
            }
            r => r?,
        };
        let presign: SnapshotPresignResponse = check_auth(resp)?.json().await?;
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
        let resp = self
            .client
            .get(format!("{}/snapshot", self.base_url))
            .bearer_auth(self.bearer())
            .header(SYNC_CAPABILITY_HEADER, SYNC_CAPABILITY_LEVEL.to_string())
            .send()
            .await?;
        let head: SnapshotHeadResponse =
            check_auth(check_capability(resp, SYNC_CAPABILITY_LEVEL).await?)?
                .json()
                .await?;
        if !head.present {
            return Ok(None);
        }
        let url = head.download_url.ok_or_else(|| {
            SyncError::Transport("snapshot head present but no download_url".into())
        })?;
        let resp = self.client.get(url).send().await?;
        // The head advertised a snapshot, but the object may be gone (e.g. a dead PUT whose
        // row outlived its bytes, or a race with relay GC). Degrade to changeset replay
        // instead of hard-failing bootstrap.
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let bytes = resp.error_for_status()?.bytes().await?.to_vec();
        Ok(Some((
            SnapshotMeta {
                generation: head.generation,
                baseline_seq: head.baseline_seq,
            },
            bytes,
        )))
    }

    async fn presign_audio(
        &self,
        sha256: &str,
        size: u64,
        part_id: &str,
        session_id: &str,
    ) -> Result<PresignResponse, SyncError> {
        let r = check_auth(
            self.client
                .post(format!("{}/audio/presign", self.base_url))
                .bearer_auth(self.bearer())
                .query(&[
                    ("sha256", sha256),
                    ("size", &size.to_string()),
                    ("part_id", part_id),
                    ("session_id", session_id),
                ])
                .send()
                .await
                .map_err(map_send_error)?,
        )?;
        Ok(r.json().await?)
    }

    async fn put_audio(
        &self,
        upload_url: &str,
        temp_path: &std::path::Path,
        content_length: u64,
    ) -> Result<(), SyncError> {
        // STREAM the encrypted temp file — never `.body(vec)` (the snapshot path's mistake,
        // transport.rs). `ReaderStream` yields ~8 KiB frames, so a 599 MB blob uploads with
        // an O(frame) working set. Content-length is pinned to match the presigned policy.
        let file = tokio::fs::File::open(temp_path)
            .await
            .map_err(|e| SyncError::Transport(format!("open audio temp: {e}")))?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        self.client
            .put(upload_url)
            .header("content-length", content_length)
            .body(body)
            .send()
            .await
            .map_err(map_send_error)?
            .error_for_status()?;
        Ok(())
    }

    async fn get_audio_streaming(
        &self,
        part_id: &str,
        dest_path: &std::path::Path,
        progress: &crate::audio::FetchProgress,
    ) -> Result<bool, SyncError> {
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering;
        use tokio::io::AsyncWriteExt;

        let resp = self
            .client
            .get(format!("{}/audio/part/{}", self.base_url, part_id))
            .bearer_auth(self.bearer())
            .send()
            .await
            .map_err(map_send_error)?;
        // Row present but blob not yet uploaded on the source device → not-yet-available.
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let resp = check_auth(resp)?; // 401 → refreshable; other non-2xx → error_for_status

        // Publish the total up front (drives the "Fetching N%" bar) when the relay/storage
        // declares a content-length; 0 stays "unknown".
        if let Some(total) = resp.content_length() {
            progress.total.store(total, Ordering::Relaxed);
        }

        let mut file = tokio::fs::File::create(dest_path)
            .await
            .map_err(|e| SyncError::Transport(format!("create audio fetch temp: {e}")))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            // Cooperative cancel: checked BEFORE each frame write so a large in-flight fetch
            // stops promptly, discarding the partial file (never a truncated cache entry).
            if progress.cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = tokio::fs::remove_file(dest_path).await;
                return Err(SyncError::Transport(FETCH_CANCELLED.into()));
            }
            let chunk = chunk?; // reqwest::Error → SyncError::Http
            file.write_all(&chunk)
                .await
                .map_err(|e| SyncError::Transport(format!("write audio fetch temp: {e}")))?;
            progress
                .received
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        file.flush()
            .await
            .map_err(|e| SyncError::Transport(format!("flush audio fetch temp: {e}")))?;
        Ok(true)
    }
}

// --------------------------------------------------------------- SSE wakeups

/// Parse ONE line of the relay's `GET /sync/stream` SSE body into a wakeup
/// `changeset_seq`, or `None` for any line that is not a `data:` payload (the
/// `event: wakeup` field line, `:`-prefixed keep-alive comments, blank inter-event
/// delimiters, or a malformed value). The relay's contract is `event: wakeup\n
/// data: <seq>\n\n`, so the load-bearing signal is the integer on the `data:` line.
///
/// The value is ONLY a hint to pull — never authoritative (see `sse.rs` on the
/// server) — so a garbled `data:` line is safely dropped rather than trusted.
#[must_use]
fn parse_sse_data_seq(line: &str) -> Option<i64> {
    // SSE permits `data:<v>` and `data: <v>`; trim either. Other fields/comments → None.
    let rest = line.strip_prefix("data:")?;
    rest.trim().parse::<i64>().ok()
}

/// Subscribe to the relay's `GET /sync/stream` SSE wakeup stream, invoking `on_wake`
/// with each `changeset_seq` the relay publishes (best-effort liveness — the caller
/// still pulls to learn the real state; the SSE value is NEVER authoritative). This is
/// the client half of the server's `sse::SseHub`.
///
/// Returns:
/// - `Ok(())` when the stream ends cleanly (the relay closed it) — the caller reconnects;
/// - `Err(SyncError::Unauthorized)` on a 401 (the bearer expired — the caller reloads the
///   drain-persisted token and re-subscribes; this fn NEVER refreshes tokens itself);
/// - `Err(SyncError::Network(_))` on a connect/timeout failure (relay unreachable — the
///   caller backs off and retries);
/// - `Err(SyncError::Http(_))` on any other transport/stream error.
///
/// The body is parsed frame-by-frame off `bytes_stream()` (O(line) working set); it never
/// buffers the whole never-ending response. The bearer is sent as an `Authorization`
/// header and is absent from any returned error (which carries at most the relay URL).
pub async fn stream_wakeups(
    client: &reqwest::Client,
    base_url: &str,
    bearer: &str,
    mut on_wake: impl FnMut(i64),
) -> Result<(), SyncError> {
    use futures_util::StreamExt;

    let resp = client
        .get(format!("{base_url}/sync/stream"))
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(map_send_error)?;
    // 401 → refreshable (caller reloads the persisted token); other non-2xx → error_for_status.
    let resp = check_auth(resp)?;

    let mut stream = resp.bytes_stream();
    // Accumulate bytes and process each complete LF-terminated line as it arrives. SSE lines
    // end in `\n` (a `\r\n` producer is tolerated by trimming the trailing `\r`).
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?; // reqwest::Error → SyncError::Http
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let text = std::str::from_utf8(&line[..line.len() - 1])
                .unwrap_or("")
                .trim_end_matches('\r');
            if let Some(seq) = parse_sse_data_seq(text) {
                on_wake(seq);
            }
        }
    }
    Ok(())
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
    // Audio: part_id → ciphertext sha; sha → stored object bytes (present objects only);
    // sha → refcount (mapping-count invariant, D8). Faithfully models existence-checked
    // presign: `already_exists` iff the OBJECT bytes are present, not just the mapping.
    audio_mappings: HashMap<String, String>,
    audio_present: HashMap<String, Vec<u8>>,
    audio_refcount: HashMap<String, i64>,
    // Sync capability gate (ADR 0006 §5): the floor the relay holds, the highest level a
    // refused raise asked for, and the devices that block a raise.
    floor: i32,
    requested: i32,
    blockers: Vec<CapabilityBlocker>,
    raise_calls: usize,
}

impl MockRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Devices the relay would report as blocking a raise (empty = every raise succeeds).
    pub fn set_capability_blockers(&self, blockers: Vec<CapabilityBlocker>) {
        self.inner.lock().unwrap().blockers = blockers;
    }

    /// Overwrite the floor, e.g. to model a relay restored from a backup.
    pub fn set_capability_floor(&self, floor: i32) {
        self.inner.lock().unwrap().floor = floor;
    }

    /// `(required, requested, raise calls so far)`.
    pub fn capability_state(&self) -> (i32, i32, usize) {
        let g = self.inner.lock().unwrap();
        (g.floor, g.requested, g.raise_calls)
    }

    /// Every stored changeset's `(client_id, client_seq)`, in commit order.
    pub fn stored_pairs(&self) -> Vec<(Uuid, i64)> {
        let g = self.inner.lock().unwrap();
        g.log.iter().map(|c| (c.client_id, c.client_seq)).collect()
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
            required_sync_capability: g.floor,
            requested_sync_capability: g.requested,
        })
    }

    async fn push_batch(
        &self,
        req: PushRequest,
        batch_capability: i32,
    ) -> Result<PushResponse, SyncError> {
        {
            let g = self.inner.lock().unwrap();
            if batch_capability > g.floor {
                return Err(SyncError::CapabilityFloor {
                    batch: batch_capability,
                    required: g.floor,
                    requested: g.requested,
                });
            }
        }
        self.push(req).await
    }

    async fn raise_sync_capability(&self, level: i32) -> Result<SyncCapabilityLevels, SyncError> {
        let mut g = self.inner.lock().unwrap();
        g.raise_calls += 1;
        if level > g.floor {
            let blocking: Vec<CapabilityBlocker> = g
                .blockers
                .iter()
                .filter(|b| b.sync_capability < level)
                .cloned()
                .collect();
            if !blocking.is_empty() {
                g.requested = g.requested.max(level);
                return Err(SyncError::CapabilityBlocked {
                    level,
                    blockers: blocking,
                });
            }
            g.floor = level;
        }
        Ok(SyncCapabilityLevels {
            required_sync_capability: g.floor,
            requested_sync_capability: g.requested,
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

    async fn put_snapshot(
        &self,
        meta: SnapshotMeta,
        ciphertext: &[u8],
        content_capability: i32,
    ) -> Result<(), SyncError> {
        let mut g = self.inner.lock().unwrap();
        if content_capability > g.floor {
            return Err(SyncError::CapabilityFloor {
                batch: content_capability,
                required: g.floor,
                requested: g.requested,
            });
        }
        g.snapshot = Some((meta, ciphertext.to_vec()));
        Ok(())
    }

    async fn get_snapshot(&self) -> Result<Option<(SnapshotMeta, Vec<u8>)>, SyncError> {
        let g = self.inner.lock().unwrap();
        Ok(g.snapshot.clone())
    }

    async fn presign_audio(
        &self,
        sha256: &str,
        size: u64,
        part_id: &str,
        _session_id: &str,
    ) -> Result<PresignResponse, SyncError> {
        let mut g = self.inner.lock().unwrap();
        // Mapping-count refcount (D8): a NEW mapping to a hash increments; a same-part
        // repeat does not.
        let same_part_same_hash = g.audio_mappings.get(part_id) == Some(&sha256.to_string());
        if !same_part_same_hash {
            g.audio_mappings
                .insert(part_id.to_string(), sha256.to_string());
            *g.audio_refcount.entry(sha256.to_string()).or_insert(0) += 1;
        }
        let present = g.audio_present.contains_key(sha256);
        Ok(PresignResponse {
            already_exists: present,
            // The mock encodes the target hash in the URL so `put_audio` can store the bytes.
            upload_url: if present {
                None
            } else {
                Some(format!("mock://audio/put/{sha256}"))
            },
            object_key: format!("mock/{sha256}"),
            content_length: size,
        })
    }

    async fn put_audio(
        &self,
        upload_url: &str,
        temp_path: &std::path::Path,
        _content_length: u64,
    ) -> Result<(), SyncError> {
        let sha = upload_url
            .rsplit('/')
            .next()
            .ok_or_else(|| SyncError::Transport("mock put: bad url".into()))?
            .to_string();
        let bytes = std::fs::read(temp_path)
            .map_err(|e| SyncError::Transport(format!("mock put read temp: {e}")))?;
        self.inner.lock().unwrap().audio_present.insert(sha, bytes);
        Ok(())
    }

    async fn get_audio_streaming(
        &self,
        part_id: &str,
        dest_path: &std::path::Path,
        progress: &crate::audio::FetchProgress,
    ) -> Result<bool, SyncError> {
        use std::sync::atomic::Ordering;
        let bytes = {
            let g = self.inner.lock().unwrap();
            match g
                .audio_mappings
                .get(part_id)
                .and_then(|h| g.audio_present.get(h))
            {
                Some(b) => b.clone(),
                None => return Ok(false), // mapping unknown or object not yet uploaded
            }
        };
        // Faithfully model the streaming transport: declare the total, then honour a cancel
        // requested before the (single) frame lands, and report received bytes on success.
        progress.total.store(bytes.len() as u64, Ordering::Relaxed);
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Transport(FETCH_CANCELLED.into()));
        }
        std::fs::write(dest_path, &bytes)
            .map_err(|e| SyncError::Transport(format!("mock get write: {e}")))?;
        progress
            .received
            .store(bytes.len() as u64, Ordering::Relaxed);
        Ok(true)
    }
}

impl MockRelay {
    /// Test helper: the current refcount the relay holds for a ciphertext hash.
    #[must_use]
    pub fn audio_refcount(&self, sha256: &str) -> i64 {
        self.inner
            .lock()
            .unwrap()
            .audio_refcount
            .get(sha256)
            .copied()
            .unwrap_or(0)
    }

    /// Test helper: whether the relay holds stored object bytes for a hash.
    #[must_use]
    pub fn audio_object_present(&self, sha256: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .audio_present
            .contains_key(sha256)
    }
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A tiny raw HTTP responder (no extra deps, no live relay): serves `responses` in order,
    /// ONE complete canned response per accepted connection. Every canned response carries
    /// `connection: close`, so reqwest opens a fresh connection per request — hence one
    /// accept per element. Each accept drains the request until end-of-headers so the
    /// client's write completes before we reply+close. Returns the bound address and the
    /// responder's `JoinHandle` so the caller keeps its `server.join().unwrap()` (a panic in
    /// the responder thread must still fail the test). The handle yields each request's
    /// head (request line and headers), in order.
    fn canned_server(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for resp in responses {
                let (mut sock, _) = listener.accept().unwrap();
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
                heads.push(String::from_utf8_lossy(&buf).into_owned());
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
            heads
        });
        (addr, handle)
    }

    /// One canned status-only response (empty body), the shape both push classifier tests serve.
    fn status_only(status_line: &str) -> String {
        format!("HTTP/1.1 {status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
    }

    fn json_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// ADR 0006 §5: the relay's capability refusals become typed errors, and a capability
    /// 409 is NEVER read as the `client_seq`-regression conflict (which drops the unacked
    /// outbox). A 409 without a capability code still is one.
    #[tokio::test(flavor = "current_thread")]
    async fn capability_refusals_are_typed_and_never_a_seq_conflict() {
        let floor = r#"{"code":"sync_capability_floor","message":"m","required_sync_capability":0,"requested_sync_capability":0}"#;
        let update = r#"{"code":"update_required","message":"Update YapStack to keep syncing","required_sync_capability":2,"requested_sync_capability":2}"#;
        let blocked = r#"{"code":"sync_capability_blocked","message":"m","required_sync_capability":0,"requested_sync_capability":1,"blockers":[{"client_id":"00000000-0000-0000-0000-000000000007","label":"old laptop","platform":"windows","sync_capability":0,"capability_seen_at":"2026-10-30T00:00:00+00:00"}]}"#;
        let (addr, server) = canned_server(vec![
            json_response("409 Conflict", floor),
            json_response("426 Upgrade Required", update),
            json_response("409 Conflict", blocked),
            status_only("409 Conflict"),
        ]);
        let t = HttpTransport::new(format!("http://{addr}"), "access-token");

        let e = t.push_batch(PushRequest::default(), 1).await.unwrap_err();
        assert!(
            matches!(
                e,
                SyncError::CapabilityFloor {
                    batch: 1,
                    required: 0,
                    ..
                }
            ),
            "got {e:?}"
        );
        assert!(
            !is_conflict(&e),
            "a capability 409 must not trigger the reseed path"
        );

        let e = t.pull(0, 10).await.unwrap_err();
        assert!(
            matches!(
                e,
                SyncError::UpdateRequired {
                    level: SYNC_CAPABILITY_LEVEL,
                    required: 2,
                    ..
                }
            ),
            "got {e:?}"
        );
        assert!(e.to_string().starts_with("Update YapStack to keep syncing"));

        let e = t.raise_sync_capability(1).await.unwrap_err();
        match &e {
            SyncError::CapabilityBlocked { level, blockers } => {
                assert_eq!(*level, 1);
                assert_eq!(blockers[0].label, "old laptop");
                assert_eq!(e.to_string(), "waiting for old laptop to update YapStack");
            }
            other => panic!("got {other:?}"),
        }

        let e = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            !is_conflict(&e),
            "a code-less 409 is not a conflict, got {e:?}"
        );
        server.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_snapshot_declares_the_content_level_and_is_refused_above_the_floor() {
        let meta = SnapshotMeta {
            generation: 1,
            baseline_seq: 0,
        };
        let relay = MockRelay::new();
        let e = relay.put_snapshot(meta, b"ct", 1).await.unwrap_err();
        assert!(
            matches!(
                e,
                SyncError::CapabilityFloor {
                    batch: 1,
                    required: 0,
                    ..
                }
            ),
            "got {e:?}"
        );
        assert!(
            relay.get_snapshot().await.unwrap().is_none(),
            "nothing recorded"
        );
        relay.put_snapshot(meta, b"ct", 0).await.unwrap();
        assert!(relay.get_snapshot().await.unwrap().is_some());

        let floor = r#"{"code":"sync_capability_floor","message":"m","required_sync_capability":0,"requested_sync_capability":0}"#;
        let (addr, server) = canned_server(vec![json_response("409 Conflict", floor)]);
        let t = HttpTransport::new(format!("http://{addr}"), "access-token");
        let e = t.put_snapshot(meta, b"ct", 1).await.unwrap_err();
        assert!(
            matches!(
                e,
                SyncError::CapabilityFloor {
                    batch: 1,
                    required: 0,
                    ..
                }
            ),
            "got {e:?}"
        );
        let heads = server.join().unwrap();
        assert!(
            heads[0]
                .to_ascii_lowercase()
                .contains("x-yapstack-snapshot-capability: 1\r\n"),
            "the presign declares the content level: {}",
            heads[0]
        );
    }

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
        let (addr, server) = canned_server(vec![
            status_only("401 Unauthorized"),
            status_only("500 Internal Server Error"),
        ]);

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

    /// End-to-end proof over a real socket that a relay `409 Conflict` (the G3
    /// counter-regression rejection) is classified by [`is_conflict`] as a conflict —
    /// distinct from a benign `500` — so the drain runs the reseed-and-recover path
    /// instead of wedging in a permanent Failing loop. A 200 is NOT a conflict. Serves the
    /// two canned statuses over a tiny raw HTTP responder (no live Postgres, no extra deps).
    #[tokio::test(flavor = "current_thread")]
    async fn push_classifies_409_as_conflict() {
        let (addr, server) = canned_server(vec![
            json_response(
                "409 Conflict",
                r#"{"code":"conflict","message":"client_seq regression"}"#,
            ),
            status_only("500 Internal Server Error"),
        ]);

        let t = HttpTransport::new(format!("http://{addr}"), "access-token");
        let e409 = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            is_conflict(&e409),
            "409 must classify as a conflict, got {e409:?}"
        );
        let e500 = t.push(PushRequest::default()).await.unwrap_err();
        assert!(
            !is_conflict(&e500),
            "500 must NOT classify as a conflict, got {e500:?}"
        );
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

    #[test]
    fn parse_sse_data_seq_isolates_data_lines() {
        // The load-bearing `data:` payload parses (with or without the space); every other
        // SSE line — the event field, keep-alive comments, blanks — and any garbled value is
        // dropped rather than trusted (the seq is only a pull hint, never authoritative).
        assert_eq!(parse_sse_data_seq("data: 42"), Some(42));
        assert_eq!(parse_sse_data_seq("data:7"), Some(7));
        assert_eq!(parse_sse_data_seq("data: 0"), Some(0));
        assert_eq!(parse_sse_data_seq("event: wakeup"), None);
        assert_eq!(parse_sse_data_seq(": keep-alive"), None);
        assert_eq!(parse_sse_data_seq(""), None);
        assert_eq!(parse_sse_data_seq("data: not-a-number"), None);
        assert_eq!(parse_sse_data_seq("data:"), None);
    }

    /// End-to-end over a real socket: `stream_wakeups` parses a canned `text/event-stream`
    /// body into the sequence of wakeup seqs and returns `Ok(())` when the relay closes the
    /// stream — the exact behaviour the desktop SSE lane loops over (reconnect on clean end).
    /// A tiny raw HTTP responder serves two events (no extra deps, no live relay).
    #[tokio::test(flavor = "current_thread")]
    async fn stream_wakeups_parses_events_until_close() {
        // Interleave a keep-alive comment + the event field lines; only the two `data:`
        // seqs must surface. Close the socket after → clean stream end.
        let (addr, server) = canned_server(vec![
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
             : keep-alive\n\nevent: wakeup\ndata: 1\n\nevent: wakeup\ndata: 2\n\n"
                .to_string(),
        ]);

        let client = reqwest::Client::new();
        let mut wakes = Vec::new();
        let r = stream_wakeups(&client, &format!("http://{addr}"), "access-token", |s| {
            wakes.push(s)
        })
        .await;
        assert!(r.is_ok(), "clean stream end must be Ok, got {r:?}");
        assert_eq!(wakes, vec![1, 2], "only the two data: seqs must surface");
        server.join().unwrap();
    }

    /// A relay `401` on the stream must surface as [`SyncError::Unauthorized`] (the caller
    /// reloads the drain-persisted bearer and re-subscribes) — never a silent hang, never a
    /// self-refresh here.
    #[tokio::test(flavor = "current_thread")]
    async fn stream_wakeups_maps_401_to_unauthorized() {
        let (addr, server) = canned_server(vec![status_only("401 Unauthorized")]);

        let client = reqwest::Client::new();
        let err = stream_wakeups(&client, &format!("http://{addr}"), "stale", |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, SyncError::Unauthorized),
            "a 401 stream must map to Unauthorized, got {err:?}"
        );
        server.join().unwrap();
    }
}
