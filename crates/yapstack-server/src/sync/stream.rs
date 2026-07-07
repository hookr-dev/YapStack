// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /sync/stream` — SSE WAKEUP stream (architecture §7a "Live push").
//!
//! Each event is `event: wakeup\ndata: <changeset_seq>` and carries ONLY the latest
//! seq — never ciphertext. It is a hint to call `GET /sync/pull`; the SSE value is NOT
//! authoritative and MUST NOT be used to advance the client's cursor. A dropped or
//! lagged stream can only fail to wake a client (covered by periodic pull +
//! completeness), never corrupt state. Single-process only (see `sse::SseHub`).

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use tokio::sync::broadcast::error::RecvError;

use crate::extract::AuthTenant;
use crate::state::AppState;

pub async fn stream(
    State(st): State<AppState>,
    auth: AuthTenant,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.sse.subscribe(auth.tenant_id);
    let s = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(seq) => {
                    // Wakeup hint only. The client MUST pull to learn the real state.
                    let ev = Event::default().event("wakeup").data(seq.to_string());
                    return Some((Ok(ev), rx));
                }
                // Lagged past the buffer: skip — the client pulls the truth regardless.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}
