# Audio Round-Trip — E2E blob sync for recordings & dictation

Grounded in a scout audit of the working tree (branch `feat/sync-relay-skeleton`) and the
owner's live library; revised after an adversarial Judge review (fail list applied in
full). Every file:line below was re-checked against the tree. Principle (from
SYNC_REMEDIATION / LIVE_SESSION_STATE): build for the production user — here, a two-device
LAN self-host — and add zero crypto or server mechanism the evidence doesn't force.

This is the oracle-amended **immediate follow-on** to the metadata-sync tranche: metadata
converges today, but the *audio never leaves the recording device*. The interim honesty
fix (unavailable-state player + error surfacing) is assumed to exist; this spec defines how
full sync supersedes it (§Slices, §Data flow).

**Load-bearing invariant:** the audio routes have **zero client callers**
(`crates/yapstack-server/src/lib.rs:82-86`) and the `audio_blobs` / `audio_objects` tables
are **empty** in every deployment. Every server-shape change below (presign existence
check, `part_id` PK/route, the new stream format and AAD) is safe *only because of this* —
no migration, no legacy blobs. This window closes the moment slice 1 ships.

## Problem

Metadata syncs; audio does not. `yapstack-sync`'s transport has **no audio methods** —
only changesets and the snapshot blob (`transport.rs:196-264`). Evidence-cited gaps:

- **The silent-play symptom.** Audio lives in per-part files
  `{session_id}.{part_index}.{wav|mp3}` under `$APP_DATA/audio` (or a custom
  `audio_save_location`). `session_audio_parts.file_path` holds the **source device's
  ABSOLUTE path**, and that string syncs verbatim. On device B the path resolves to nothing
  (or, worse, to an unrelated local file). Legacy `sessions.wav_file_path` /
  `dictation_history.wav_file_path` are backfilled to `part_index=0`, same problem. Result
  today: B opens a synced session and the player silently has nothing to play. The interim
  fix turns that into an honest "unavailable"; this spec turns it into "fetching…" → plays.

- **Scale reality (owner's Mac).** 2,329 audio files, 2.7 GB total; largest single WAV
  **599 MB** (others 190 / 124 MB). A fresh device auto-pulling 2.7 GB is hostile — and the
  existing AEAD is strictly one-shot: `seal_standard`/`open_standard` take the whole
  plaintext/blob as a single slice (`crates/yapstack-crypto/src/aead.rs:40-82`). A
  whole-blob envelope therefore **cannot** be constant-memory without new crypto; that
  forces the streaming construction in D5, it cannot be implemented away.

- **The seam exists and is proven.** `POST /audio/presign?sha256=&size=&session_id=` and
  `GET /audio/{session_id}` (`audio.rs:57,206`) already do content-addressed dedup within a
  tenant on the **ciphertext** hash + a refcount (`audio.rs:7-9`), pin content-length in the
  presigned policy (`audio.rs:196`), meter only *new* bytes through the single choke point
  (`audio.rs:190-192`), and never see plaintext or log URLs (`audio.rs:1-5`). MinIO is
  configured and healthy in the UAT env (bucket `yapstack`, LAN public endpoint). The
  snapshot path in `transport.rs:196-230` is the presign→direct-PUT pattern to mirror —
  with one correction: it buffers the whole ciphertext (`.body(ciphertext.to_vec())`,
  `transport.rs:225`); audio PUT/GET must use **streaming bodies** (§Data flow).

- **Crypto is half-built.** `DOMAIN_AUDIO` `b"yapstack.audio.v1"` exists as a **domain
  constant only** (`CRYPTO_SPEC.md:828`) with an AAD definition (`:428-429`). There is **no
  audio round-trip KAT** — the §13.4 standard-envelope vector is **changeset-domain**
  (`CRYPTO_SPEC.md:883`). Audio vectors must be *generated* (§Verification). No client blob
  cipher wrapper exists.

**Three seam defects the Judge surfaced (all closed below):**

1. **Silent loss on failed PUT (HIGH).** Presign commits the blob row and meters the choke
   **before any bytes land** (`audio.rs:150-196`). If the direct PUT then dies, a retry
   presign hits the dedup path (`audio.rs:128-145`) → `already_exists`, no `upload_url`,
   refcount inflated — the client marks the upload done and **the object never exists**.
   Fix: server-side object-existence check (D8).
2. **Part-identity collision (MED).** The CRR rebuild **strips**
   `UNIQUE(session_id, part_index)` (merge hazard; pinned by test
   `crates/yapstack-sync/src/schema.rs:457-476`), so two devices can mint the *same*
   `part_index` for one session. Addressing blobs by `(session_id, part_index)` is
   therefore unsound cross-device. Fix: address by the part row's UUID (D6).
3. **Memory/crypto contradiction (HIGH).** See scale-reality bullet; fixed by D5.

## Decisions

| # | Decision | Rationale / evidence |
|---|---|---|
| D1 | **Blobs are the source of truth; `file_path` is demoted to a local-only, cross-device-untrusted legacy hint.** Resolution is by part identity, never by the synced absolute path. | The synced `file_path` is a source-device absolute path — meaningless on B. Same trust posture as the "untrusted server hints" rule (`CRYPTO_SPEC.md:456-464`): usable as a *local same-device* shortcut, never for a cross-device decision. |
| D2 | **Resolution order** (§Data flow): (1) local cache by ciphertext sha; (2) `file_path` **only if** it resolves under *this* device's audio dir and the file exists (same-device legacy fast-path); (3) fetch-on-demand from relay. Custom `audio_save_location` is a **local** setting: resolution and the fetch cache use *this* device's dir, never the source device's. | Keeps single-device behaviour byte-identical (path hit), makes cross-device correct (fetch), never trusts a foreign path. |
| D3 | **On-demand fetch is the default**: click play → GET → decrypt-to-cache → play, with progress UI. No full mirroring. | 2.7 GB auto-pull on a fresh device is hostile. "Keep all audio on this device" is an opt-in **later slice / follow-on** (§Slices S4), not v1 default. |
| D4 | **Background upload queue on session finalize and dictation save.** Durable (survives restart), content-addressed by ciphertext sha256, resumable at whole-blob granularity (retry = re-presign + re-upload; dedup + D8 make it idempotent). **Recording is never blocked by upload.** Two priorities: normal (new recordings) and a **low-priority backfill lane** (D9) drained only when the normal lane is empty; the uploader throttles to one in-flight blob at a time so drain and UI never starve behind a 599 MB upload. | Mirrors the outbox pattern but is an **independent lane** — audio is best-effort/background; changesets are the correctness-critical lane. Ordering between the two is independent (§Data flow). |
| D5 | **Chunked STREAM encryption is the ONE audio format** (`yapstack.audio.stream.v1`), built on the **maintained RustCrypto implementation** (`chacha20poly1305::aead::stream`, i.e. the `aead` crate's STREAM module — `aead 0.5.2` is already in-tree via `chacha20poly1305 0.10.1`; enable its `stream` feature), **never hand-rolled**. It fully replaces the whole-blob standard envelope for audio — **no dual format**. | The one-shot API (`aead.rs:40-82`) makes constant-memory whole-blob impossible, and hand-rolling incremental AEAD is forbidden. STREAM gives true **O(chunk) memory both directions**, per-chunk positional binding via the nonce counter, and last-block truncation detection — and its counter-addressable segments enable future range access (follow-on, NOT v1). This is **new, Judge-gated crypto**: the CRYPTO_SPEC amendment draft (§Crypto) must pass adversarial crypto review before the wrapper lands. Playback-UX consequence: v1 still fetches the full blob before playing (no range wiring), so time-to-first-audio for the 599 MB WAV = full LAN transfer + streaming decrypt; the format makes progressive playback possible later without re-encrypting. |
| D6 | **Address blobs by the part row's UUID** `session_audio_parts.id` (`TEXT PRIMARY KEY`, already the client PK; `schema.rs:461-462`): server `audio_objects` keys `(workspace_id, part_id)`, route `GET /audio/part/{part_id}`, presign takes `part_id`; identity AAD binds `part_id`. `session_id` becomes a server-side **metadata column** for future per-session listing. **MANDATE (ships in S1, before any blob exists): switch part-id minting to CSPRNG UUIDv4.** Today `insert_audio_part_row` derives the id from the clock, twice (`apps/desktop/src-tauri/src/db.rs:284-288`, `rand_u64_from_clock` at `:309-316`); only the v15 backfill uses `randomblob(16)` (`db.rs:973`). Advisory: ids are **32-hex simple format** (no dashes) — the server route must accept simple-format UUIDs. | The CRR rebuild strips `UNIQUE(session_id, part_index)` (`schema.rs:457-476`), so `(session_id, part_index)` can collide across devices. Row UUIDs make collisions structurally impossible — but **only once minting is CSPRNG**: two NTP-synced LAN devices minting clock-derived ids are NOT structurally collision-free, and a cross-device PK collision under cr-sqlite would **merge two parts**. With the one-line CSPRNG change the structural claim holds and no LWW rule is needed. Safe now only because the tables are empty (load-bearing invariant above). |
| D7 | **Cellular/metered-network policy is N/A** for the current LAN self-host deployment; no per-network gating in v1. | Primary (only) deployment today is two-device LAN. Revisit if a hosted/WAN relay ships. |
| D8 | **Presign verifies object existence before claiming `already_exists`.** Invariant: **refcount = the number of `audio_objects` mappings referencing the hash; increment/decrement exactly on mapping create/repoint (as the server does today), independent of object existence.** The HEAD existence check gates **ONLY** the `already_exists`/`upload_url` decision: object **present** → `already_exists=true`; object **absent** (row committed but PUT died) → return a fresh `upload_url`. **Choke metering stays first-presign-only** (a D8 re-presign never re-meters; bytes were reserved when the blob row was created). Client-side rule: the upload queue never trusts `already_exists` for a blob **this device is itself mid-uploading** unless the server's existence check backs it; an entry is marked `done` only on (a) a 2xx direct PUT, or (b) `already_exists` from an existence-checking server. | Closes the silent-loss-on-failed-PUT hole (`audio.rs:150-196` commits row + meters choke pre-bytes; retry then hits `:128-145` with no `upload_url`). The refcount invariant must be mapping-count-based, NOT existence-based: the mapping upsert is unconditional (`audio.rs:116-126`), so if device B presigns identical content for a **different part** while A's object is still absent, B's mapping is created — an existence-gated increment would count nothing and refcount would **permanently undercount** (GC would later delete a blob B still references). Same-part retries hit the fully-idempotent branch (`audio.rs:88-99`) and never double-count. The HEAD is metadata-only — the relay still never sees bytes or plaintext (`audio.rs:1-5`). Small server delta, safe under the empty-tables invariant. |
| D9 | **Historical audio IS included — one-time backfill-enqueue slice, default ON.** Walk local `session_audio_parts` rows whose files exist on this device and enqueue them on the low-priority lane, behind all new recordings. The walk is **re-runnable and idempotent**: enqueue is INSERT-OR-IGNORE by `part_id` and walk-completion is recorded, so a restart mid-walk resumes/re-runs safely. **Failed queue entries retry on app start plus manual retry from the status surface.** Advisory: queue entries whose part row was deleted before upload are dropped silently, not surfaced as errors. | On-demand fetch (D3) means peers download only what they play, so the total cost is a single upload pass on the recording device over LAN (2.7 GB, background, throttled per D4). Without backfill, D1's "blobs are truth" would be false for the entire existing library. |
| D10 | **Deletes, v1 posture (explicit):** session/part **row** deletion syncs today via the CRDT. Blob release (refcount decrement + object delete) is **DEFERRED to the server GC follow-on** (already a documented stub, `audio.rs:8-9`) — deleted sessions' blobs **persist server-side until GC lands**. Cross-device delete-vs-mid-fetch resolves as *fetch completes, then the local delete applies*: the rows are gone, so the UI never shows the audio; the orphaned cache entry is reclaimed by cache policy. | Honest bounded scope: no new server mechanism, no distributed delete protocol; the only cost is temporary server-side storage of unreferenced ciphertext, reclaimed when GC ships. |

## Data flow

**Blob container (`yapstack.audio.stream.v1`).** Per `CRYPTO_SPEC §4` (`:336-338`), each
blob gets a **fresh random 32-byte data key**, wrapped under `vault_key` with the existing
**committing envelope** (§4.2). Identity is bound **at the wrap layer** (D6):

```
audio_blob = LP(wrapped_data_key)  # committing envelope §1.4/§4.2
                                   # wrap AAD = LP(version, "yapstack.wrap.audio.stream.v1",
                                   #               tenant_id, session_id, part_id, epoch_u32)
          || header               # version(1)=0x01 || chunk_size(u32be) || nonce_prefix(19)
          || seg_0 … seg_n        # STREAM segments (see §Crypto for exact semantics)

stream plaintext = codec_tag(1: wav|mp3) || original_audio_bytes   # codec stays encrypted
```

The **ciphertext hash content-addressed and uploaded is the whole `audio_blob`**. Framing
is pinned by generated vectors + round-trip tests in slice 1 (§Verification).

**Upload path:**
1. On session finalize / dictation save, enqueue `(part_id, source_file, priority=normal)`
   in the durable local `audio_upload_queue` (local-only table, NOT CRR'd; state machine
   `pending → sealing → uploading → done | failed`, with `attempts`). The backfill pass
   (D9) enqueues at `priority=low`; low drains only when normal is empty; one in-flight
   blob at a time (D4).
2. Background uploader: streaming seal to a temp file (O(chunk) memory), computing `sha256`
   + `size` in the same pass → `POST /audio/presign?sha256=&size=&part_id=`.
   `already_exists` (existence-checked per D8) → done, no bytes moved. Else PUT the temp
   file to the presigned URL with a **streaming body** (`reqwest::Body::wrap_stream` over
   the encrypted temp file — NOT the snapshot path's `.body(vec)`, `transport.rs:225`),
   content-length pinned.
3. Retry = re-presign + re-upload; D8 guarantees a dead PUT yields a fresh `upload_url`,
   never a phantom `done`. Recording never waits on this lane.
4. **Failure surfacing:** upload-lane failures feed the **same drain-health/status surface**
   as changeset sync (distinct lane label) — never silent, per repo posture (SYNC_REMEDIATION
   F2).

**Fetch path:**
1. Resolve part in D2 order. Cache/path hit → play immediately.
2. Miss → `GET /audio/part/{part_id}` → 302 presigned GET (`audio.rs:206-231` shape) →
   stream to a temp file. UX: progress + **cancel**, a **disk-space precheck** against the
   blob's `content_length` before starting, and **concurrent-fetch coalescing** — a single
   in-flight fetch per blob that all subscribing views share. "Fetching…" replaces the
   interim "unavailable" state.
3. Streaming open: unwrap data key (wrap AAD must verify for *this* `part_id`), decrypt
   segment-by-segment (truncation/reorder rejected by STREAM semantics), strip `codec_tag`,
   promote to the local cache keyed by ciphertext sha. Play. Failure → surface verbatim; no
   fallback, no auto-route (privacy posture).

**Ordering vs the changeset outbox:** independent lanes. A session row/segment can converge
before its audio uploads; the fetch path handles "row present, blob not yet uploaded"
(GET 404 → "not yet available on the source device"). Deletes per D10.

## Crypto — CRYPTO_SPEC amendment draft (ONE amendment, Judge-gated)

One amendment covers **both** the stream format and part-identity binding. It must pass
adversarial crypto review (repo rule: no self-certified security) before slice 1's wrapper
lands. Draft content:

- **New domain `yapstack.audio.stream.v1`** (add to the §12 domain table). It **replaces**
  `yapstack.audio.v1` for audio entirely — the old domain's AAD line
  (`CRYPTO_SPEC.md:428-429`) is marked *retired, never shipped* (zero blobs exist;
  load-bearing invariant). No dual format.
- **Construction:** RustCrypto STREAM (`aead::stream`, `StreamBE32`) over
  XChaCha20-Poly1305 with a fresh per-blob data key. Per-blob 19-byte random
  `nonce_prefix`; per-segment nonce = `nonce_prefix || counter(u32be) || last_flag(1)` —
  supplied by the crate, **never hand-assembled**. Position binding = the counter;
  truncation detection = the final segment is sealed with `last_flag` set and opened via
  the crate's `decrypt_last`, so a truncated or extended stream fails authentication.
- **Chunking:** `chunk_size` = 1 MiB plaintext per segment (v1 constant, recorded in the
  header for agility); each segment = `ct || tag(16)`; last segment may be short. Memory
  bound: O(chunk_size) in both directions, independent of blob size (599 MB WAV ⇒ ~1 MiB
  working set + I/O buffers).
- **Header authentication:** the clear header (`version || chunk_size || nonce_prefix`) is
  passed as **AAD on every segment**, so header tamper (e.g. a `chunk_size` flip) fails the
  first `open`. Version byte remains the first authenticated field (C1 discipline,
  `CRYPTO_SPEC.md:421-424`).
- **Identity binding at the wrap layer** (PM decision): wrap AAD =
  `LP(version, "yapstack.wrap.audio.stream.v1", tenant_id, session_id, part_id, epoch_u32)`
  on the committing data-key wrap. `epoch_u32` (the vault-key rotation epoch, big-endian)
  is REQUIRED by LOCKED §4.2: every wrapped data key binds it so a key wrapped under vault
  generation N cannot be silently reinterpreted under N+1 (`CRYPTO_SPEC.md:352-358`). A
  blob replayed under another tenant/session/part/epoch fails at unwrap, before any segment
  is touched. `part_id` (not `part_index`) per D6.
- **KAT plan (§13 addition — vectors must be GENERATED; none exist today):** the only
  audio artifact in the spec is the domain constant (`CRYPTO_SPEC.md:828`); the existing
  standard-envelope vector §13.4 is **changeset-domain** (`:883`) and proves nothing about
  audio. Generate: (1) a fixed-key, fixed-prefix 3-segment vector (short last segment) with
  full hex; (2) round-trip assertions in both stacks; (3) **truncation rejection** (drop
  the last segment → open fails); (4) **reorder rejection** (swap two segments → open
  fails); (5) wrap-AAD mismatch — both a wrong-`part_id` vector AND a wrong-`epoch_u32`
  vector (each → unwrap fails).
- **Untrusted-hint discipline:** server metadata columns and the synced `file_path` remain
  hints only; the AEAD tags + wrap AAD are the authority (`CRYPTO_SPEC.md:456-464`).
- **Range access:** counter-addressable segments permit future ranged decrypt from segment
  boundaries; explicitly **not wired in v1** (§Non-goals).

## Server-side deltas

Routes exist; deltas are **minimal but non-zero**, all safe under the empty-tables
invariant (top of doc):

- **Part-identity keying (D6).** `audio_objects` PK → `(workspace_id, part_id)`, with
  `session_id` kept as a metadata column; `presign` takes `part_id`; add
  `GET /audio/part/{part_id}`. Dedup/refcount/choke logic (`audio.rs:71-202`) is otherwise
  unchanged (still content-addressed on the ciphertext hash).
- **Existence-checked presign (D8).** Dedup-hit path gains a storage HEAD; absent object →
  fresh `upload_url`, no re-metering; refcount follows the D8 mapping-count invariant
  (a NEW mapping still increments even while the object is absent — only same-part
  retries don't). Add the matching integration tests (§Verification).
- **GC stays a stub** (`audio.rs:2,8-9`): refcount-0 soft-delete GC and delete-driven
  refcount decrement are the **server follow-on** (D10, §Non-goals).
- Nothing else: no plaintext, no re-hash, no blob bytes through the relay (`audio.rs:1-5`).

## Slices (reversible, smallest-first)

- **S1 — engine-only, no UI (testable headless).** The CRYPTO_SPEC amendment draft + Judge
  crypto review (gates the rest of S1); stream cipher module (`aead::stream`-based seal/open
  to/from files, header, wrap); transport methods (`presign_audio` / `put_audio` /
  `get_audio`) with **streaming bodies**; durable two-priority `audio_upload_queue` +
  background uploader on finalize/dictation-save with drain-health surfacing; server deltas
  (part_id keying + existence-checked presign); **CSPRNG UUIDv4 part-id minting** (D6
  mandate — replaces the clock-derived id at `db.rs:284-288`). Ships every §Verification
  row except the cross-device UAT.
- **S2 — backfill-enqueue (D9).** One-time walk of local `session_audio_parts` with
  existing files → low-priority lane; default ON; status surface shows backfill progress.
- **S3 — playback fetch-on-demand + UI.** D2 resolution order, GET→decrypt→cache→play with
  progress/cancel/disk-precheck/coalescing; **supersedes the interim "unavailable" with
  "fetching…"**. Ships the cross-device UAT (record A → play B).
- **S4 — settings + optional mirror.** Custom `audio_save_location` wired to the fetch
  cache; opt-in "keep all audio on this device" background mirror (default OFF). The mirror
  may defer to a follow-on.
- **Follow-ons (not v1):** server GC + delete-driven refcount decrement (D10); ranged /
  progressive playback atop the stream format (D5).

## Verification

| Check | What it asserts | Slice |
|---|---|---|
| Stream-format KATs (**generated** — none exist; only the domain constant at `CRYPTO_SPEC.md:828`, and §13.4 is changeset-domain `:883`) | 3-segment vector reproduces in both stacks; **truncation rejected**; **reorder rejected**; wrap-AAD `part_id` and `epoch_u32` mismatches rejected | S1 |
| Client round-trip | streaming seal → upload → download → streaming open is **byte-equal** to the source WAV/MP3, including a >200 MB file with a bounded-memory assertion (O(chunk)) | S1 |
| Re-presign after dead PUT (D8) | presign → kill the PUT → re-presign returns a fresh `upload_url` (not `already_exists`), refcount NOT inflated; queue entry never falsely `done` | S1 |
| No-plaintext canary → object storage | canary WAV with a known marker; assert the **MinIO object bytes** contain no marker (extends the DB no-plaintext grep to storage) | S1 |
| Dedup | same content twice → **one blob, refcount 2** (server dedup test exists per board §12c; extend for D6/D8 + client path). **Concurrent case:** two parts, same content, second presign issued **while the object is still absent** → mapping created, `upload_url` returned, and refcount ends at **2** (mapping-count invariant, D8) | S1 |
| Choke meter | new-blob presign metered once at declared size; dedup hit and D8 re-presign meter nothing | S1 |
| Upload-lane surfacing | induced upload failure appears in the drain-health/status surface; never silent | S1 |
| Backfill | fresh queue + populated library → all local parts enqueued low-priority; a new recording jumps the lane | S2 |
| Cross-device UAT | record on A → open on B → "fetching…" (progress, cancellable) → **plays**; human gate, never self-certified | S3 |

## Non-goals (v1)

Transcoding; ranged/progressive playback (format supports it; wiring is a follow-on);
share-link audio; server GC + delete-driven blob release (D10 follow-on); "keep all audio"
default-on mirror (opt-in, S4/follow-on); iOS/web; cellular/metered gating (D7, N/A on
LAN).

## Resolved questions (owner, 2026-07-17)

1. **Keep-all-audio default — on-demand fetch confirmed, NO mirroring.** Owner: *"A machine
   should pull audio files it doesn't have from the server on demand, no need to mirror
   everything."* The S4 keep-all-audio mirror stays a deferred opt-in. The owner added a
   sharpening that becomes an explicit invariant:

   > **SERVER COMPLETENESS INVARIANT** (owner: *"The server should have the full history
   > always though so it can be served."*) — every device treats audio that exists locally
   > but not on the server as an **outstanding upload debt**. Enforcing mechanism: the
   > durable upload queue (finalize-enqueue, D4) + the re-runnable idempotent backfill walk
   > (D9) — and the walk applies on **EVERY device** (any device that ever recorded
   > locally), not only the historical back-catalog device. A periodic/on-demand
   > completeness audit (compare local parts vs server mappings) is a cheap follow-on
   > hardening, not v1-blocking.

2. **Cache eviction — keep-until-manual-clear** (owner: follow recommended). Accepted
   consequence, as noted in D10: delete-orphaned cache entries persist until manual clear.
3. **Backfill timing — starts immediately when the tranche ships, with visible progress**
   (owner: follow recommended). No confirmation gate.
