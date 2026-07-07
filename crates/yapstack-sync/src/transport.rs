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
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;
use yapstack_common::sync::{
    ClientTail, CompletenessResponse, PullResponse, PulledChange, PushAck, PushRequest,
    PushResponse,
};

use crate::SyncError;

#[async_trait]
pub trait SyncTransport: Send + Sync {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError>;
    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError>;
    async fn completeness(&self) -> Result<CompletenessResponse, SyncError>;
}

// --------------------------------------------------------------- HTTP transport

/// Real relay client. `base_url` is the server root (e.g. `https://sync.example`);
/// `bearer` is the access token from the auth flow (CRYPTO_SPEC §3).
pub struct HttpTransport {
    base_url: String,
    bearer: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: bearer.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl SyncTransport for HttpTransport {
    async fn push(&self, req: PushRequest) -> Result<PushResponse, SyncError> {
        let r = self
            .client
            .post(format!("{}/sync/push", self.base_url))
            .bearer_auth(&self.bearer)
            .json(&req)
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
    }

    async fn pull(&self, since: i64, limit: i64) -> Result<PullResponse, SyncError> {
        let r = self
            .client
            .get(format!("{}/sync/pull", self.base_url))
            .bearer_auth(&self.bearer)
            .query(&[("since", since), ("limit", limit)])
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
    }

    async fn completeness(&self) -> Result<CompletenessResponse, SyncError> {
        let r = self
            .client
            .get(format!("{}/sync/completeness", self.base_url))
            .bearer_auth(&self.bearer)
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
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
}
