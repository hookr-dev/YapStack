# Glossary

Canonical names for things in YapStack. Use these in code, commits, and tickets so search and grep are reliable.

## Capture & audio

- **Mic** — microphone capture stream from a `cpal` input device.
- **System audio** — speaker/output capture stream (loopback on macOS via cpal output device).
- **Mixed** — combined mic + system audio capture.
- **Capture source** — `MicOnly | SystemOnly | Mixed` (`CaptureSource` enum, `CaptureSourceDto` for IPC).
- **Ring buffer** — lock-free SPSC `AudioRingBuffer` (UnsafeCell + atomics). Producer is the cpal callback; consumer is the live transcription loop.
- **`write_pos`** — monotonic byte cursor on the ring buffer, never resets. Cursors snapshot relative positions for `extract_since(pos)`.
- **`peek_energy_rms`** — non-consuming RMS over the last N samples. Used by the recording-indicator UI.

## Transcription

- **Engine** — `Whisper | Parakeet`. User-selectable in settings; backed by separate sidecar features.
- **Sidecar** — `yapstack-transcription-sidecar` binary. Spawned by the desktop app; communicates over JSON-line IPC on stdin/stdout. Logs to stderr.
- **IPC protocol** — tagged JSON unions (`SidecarRequest::Transcribe`, `SidecarResponse::Transcription`, etc.). One `id: u64` per request for correlation.
- **VAD (voice activity detection)** — Silero V5 ONNX model, shared singleton. Per-source state machines emit speech/silence transitions.
- **Backfill** — historical audio from before recording started, transcribed concurrently with the live stream. Segments carry `origin: "backfill"`. The scheduler drains backfill at the lowest priority, behind every other tier.
- **Scheduler** — `TranscriptionScheduler`. Long-lived single-worker priority queue in front of the sidecar lane (`commands/transcription_scheduler.rs`), constructed once at engine init and shared by every live runtime. Priorities: `FinalFlush > Dictation > Live > Backfill`, with mic/system round-robin at the Live tier. Sole caller of `transcribe_with`. Backfill is gated by a per-producer busy bitmask `{LiveMic, LiveSystem, Dictation}` — admitted only when every bit is clear.
- **Final flush** — closing-words chunks emitted at session stop. Submitted at `FinalFlush` priority so they outrank pending Live, Dictation, and Backfill work and survive the stop path even if backfill is still draining.
- **Job origin** — `JobOrigin::Live | Dictation | Backfill | FinalFlush`. Scheduler *priority class* — which queue lane a job occupies, not which runtime produced it. A dictation runtime's stop-time chunks are still legitimately `FinalFlush`. Strictly distinct from **Source kind** (the routing identity); frontend filtering is on source kind, never on origin. Mirrored on the wire as `SegmentOrigin` in `LiveSegmentEvent.origin`.
- **Source kind** — `LiveSourceKind::Session | Dictation`. Routing identity of a live-transcription runtime, threaded onto every emitted event (segment / status / backfill-complete) and accepted on `start_live_transcription` / `stop_live_transcription` / `get_live_transcription_status`. Frontend listeners filter on source kind + `session_id` (two-prong) so dictation lifecycle never leaks into session UI state and stale events from a recovered session can't clobber a freshly-active one.
- **Runtime slot** — one of two named slots (`session`, `dictation`) on `LiveTranscriptionState`, each carrying explicit `Idle | Starting | Running | Stopping` lifecycle. Stays non-`Idle` until the spawned task's finalizer transitions it back, so same-kind double-start is rejected even during finalization. Engine shutdown (`shutdown_transcription_client`) is gated on every slot being `Idle`.
- **Mic-ownership window** — durable `[start, end)` mic ring-buffer interval recorded by `DictationOwnsMic` for each dictation. Dictation start opens a window with the captured `start_pos` (also threaded into the dictation runtime's mic VAD cursor + WAV flush position so audio between hotkey press and live-loop spawn is captured); the dictation finalizer reads the final mic position and closes the window. The session loop drives off the closed-window list, so a dictation that opens-and-closes between two session polls still appears as a closed window and is consumed cleanly. Replaces the prior boolean+atomic flag, which had a fundamental race with sub-poll-interval dictations.
- **Prompt context** — Whisper-only feature: prior text fed into the next inference window for continuity. Decays after silence.
- **Diarization** — speaker labelling. Parakeet + Sortformer post-pass only. Produces `speaker_id: Some(u8)` on segments.
- **Hallucination filter** — drops empty / token-only / known-pattern segments (e.g., `"thank you"`) at low confidence.

## Storage

- **Session** — one recording session. Has a UUID, status, and zero or more parts.
- **Segment** — one transcribed snippet with timestamps. Persisted in the `segments` table.
- **Part** — one continuous audio file segment of a session. A session has N parts (resumed sessions add parts). Stored as WAV (then optionally re-encoded to MP3 on stop). Tracked in `session_audio_parts`.
- **Note** — Tiptap rich-text doc associated with a session. `notes` table; versions in `note_versions`.
- **Folder** — primary organizational primitive. Hierarchical, with icons and colors. Folder names flow into AI vocabulary hints.
- **Tag** — flat metadata applied by AI during summarization. Lighter than folders.
- **Dictation** — short voice-to-text utterance, processed via a per-slot system prompt and routed to paste/copy/note. Distinct from sessions; persisted in `dictation_history`. Can run concurrently with an active session — the dictation runtime opens a **mic-ownership window** that the session honors (its own mic-side dispatch suspends, WAV mutes the overlap) so dictated audio doesn't appear in the session transcript or recording.
- **Slot** — named dictation configuration with its own keybind, mode (hold/toggle), AI prompt, and output action.
- **Volume duck** — temporarily lowering the system output volume during a dictation so you can hear yourself over earphone playback. Snapshots `(device_id, prior_level)` so the *original* device is restored even if the default output changes mid-duck. macOS only; no-op elsewhere. Mechanism in `apps/desktop/src-tauri/src/system_volume.rs`; Tauri surface in `commands/system_volume.rs`.

## Auto-updater & release

- **Updater pubkey** — public Ed25519 minisign key in `tauri.conf.json`. Signature verified against the bundled `.app.tar.gz.sig`.
- **`latest.json`** — Tauri update manifest produced by `release.yml`. Lives at `releases/latest/download/latest.json`.
- **Draft release** — GH release in draft state, artifacts uploaded but not visible to the public; URLs resolve only after publish.

## Frontend

- **Live controller** — the long-running async loop in `commands/live_transcription.rs` that pulls audio, runs VAD, dispatches to the sidecar, and emits segment events.
- **Pressure event** — telemetry log + frontend event marking real-time-factor (RTFx) and stream lag. Used to detect "transcripts falling behind."
- **Vocabulary hints** — folder/tag-derived prompt prefix sent to Whisper; updated mid-recording via `update_vocabulary_hints`.

## CI & process

- **Verification** — `pnpm check`. The single command that gates merge.
- **CHANGELOG entry** — required for any user-visible change. Added under `## [Unreleased]`.
- **ADR (architecture decision record)** — short doc in `docs/adr/` capturing a structural decision and its rationale. Append-only; supersede rather than edit.

## Sync

Full specification: [`SYNC_SPEC.md`](SYNC_SPEC.md). Cryptographic terms are normative in [`CRYPTO_SPEC.md`](CRYPTO_SPEC.md).

- **Relay** — the self-hosted blind server (`crates/yapstack-server`). Stores ciphertext and opaque metadata; never decrypts, never merges; its only outbound calls are a metadata HEAD and a GC DELETE against the operator's own object storage.
- **Vault key** — the per-account random key that (via per-item data keys) encrypts all content. Wrapped under the password-derived master key and, independently, the recovery code.
- **Recovery code** — 160-bit system-generated code shown once at signup (`AAAA-BBBB-…`, 8×4). Unwraps the vault key when the password is lost. Lost password + lost code = unrecoverable data.
- **Device identity** — per-install `client_id` (UUIDv4) + Ed25519 keypair, kept in the OS keychain; survives sign-out.
- **Device fingerprint** — `SHA-256(ed25519_pub)` → base32, 4×4 groups. Shown on both screens during approval.
- **Roster** — the signed device list (counter, epoch, members) authored by a vault-key holder; the cryptographic source of truth for membership. The relay stores it opaquely and enforces only that the counter strictly increases.
- **Security code** — the roster fingerprint plus vault-key epoch ("Security code XXXX-… · key vN"), compared out-of-band at approval.
- **Pending / active / revoked** — the relay's device states. A device that has signed in but not yet been approved is *pending* (it may read, not write); an approver's re-signed roster promotes it to *active*; a device another device removed is *revoked* (its tokens are dead, its writes refused; signing in again re-enrols it pending). Only active devices may write the roster or turn sync on — with one exception, the recovery grant.
- **Proof of possession** — at sign-in and recovery the device signs a one-time challenge (nonce) from the relay with its Ed25519 device key, so holding the password alone cannot act as another device (CRYPTO_SPEC A10; relay protocol version 2).
- **Recovery grant** — a one-shot, 15-minute server-side mark set by a recovery-code sign-in that lets a still-pending device admit itself to the roster once, so a user who lost every other device can get back in (CRYPTO_SPEC A7).
- **Removed device** — a device evicted from the account by another device's re-signed roster (state *revoked*). It keeps what it already synced; a password holder can still read new changes until the password is changed.
- **Account anchor** — per-(server, email) keychain entry that survives sign-out and holds the salt and roster-counter baselines, so a returning device is *known*, not trust-on-first-use.
- **Cutover** — turning the live database into the CRR-tracked database (staging copy, CRR migration, journaled file swap with backup and row-count oracle).
- **Drain** — the background loop that pushes the outbox and pulls peer changesets every 5 s, plus SSE wakeups and local-write kicks.
- **Outbox** — local table of encrypted, row-atomic changeset chunks awaiting relay acknowledgement.
- **Watermark** — `push_watermark` (last captured local db_version) and `pull_watermark` (last merged relay `changeset_seq`).
- **Quarantine** — two kinds: *schema quarantine* (rows for a column this device lacks; replayed after migration) and *crypto quarantine* (changesets that failed to decrypt; durable, surfaced to the user as "N changes unreadable", retryable).
- **Catching up** — outbox empty but the pull is still behind the relay tip; never rendered as "Up to date".
- **Audio lane** — the independent upload queue + uploader for encrypted audio blobs (normal priority for new recordings, low priority for the library *backfill*), and the fetch-on-demand path with its local *fetch cache*.
- **Remote-live session** — a session recording on another device, rendered read-only with a "Live on <device>" badge; "Interrupted" after 3 min without new segments.
