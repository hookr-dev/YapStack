<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# YapStack Sync — System Specification (v1, as built)

> **Status:** Descriptive-normative, 2026-09-10; aligned with the code as built for
> merge on 2026-09-29 (§14 rows carry their landing commits, §17 is executed). This
> document specifies the sync system **as it exists on `feat/sync-relay-skeleton`** and is the single in-repo
> reference for what the system does, what it promises the user, and where the
> implementation departs from the earlier design documents. Cryptographic
> constructions remain normative in [`CRYPTO_SPEC.md`](CRYPTO_SPEC.md); this document
> never restates a LOCKED construction, it cites it.
>
> **Why this exists.** The design research was fragmented: the architecture decision
> report (`YAPSTACK_SYNC_ARCHITECTURE.md`, "World B") lives outside the repository and
> is what every `architecture §N` reference in code comments and in CRYPTO_SPEC points
> at; the remediation plan and adversarial review findings were untracked working files;
> three plan documents under `docs/plans/` each describe a slice. §13 folds those into
> one place and §14 records every divergence so the simplification pass that follows can
> cut against a spec instead of against folklore.
>
> **How to read it.** §1–§2 are the product contract. §3–§11 are the system as built,
> one subsystem per section, with file pointers. §12 lists the invariants the tests
> pin. §13 maps the old documents onto this one. §14 is the divergence register.
> §15 records the owner's decision on each edge case (ratified 2026-09-10) and §17 the
> resulting plan — a fixed-scope simplification pass, then the feature slices — with what
> landed and what is still held for a decision.

---

## 1. Product posture and user-facing contract

### 1.1 Posture (unchanged from the decision report)

1. **On-device merge, blind relay, end-to-end encrypted.** Every device runs the
   cr-sqlite CRDT engine and merges locally. The relay stores and fans out opaque
   encrypted changesets and encrypted audio blobs. It never decrypts, never runs the
   CRDT engine, and makes no outbound calls except to the storage and database the
   operator configures.
2. **No server-side content features.** No hosted search, no server AI over content,
   no server-rendered pages.
3. **Self-host is the maximum tier.** No feature gating; entitlements are quantitative
   limits only and resolve to unlimited on self-host.
4. **Password-only auth in v1.** OAuth is deferred because it yields no key material.
5. **Lost password + lost recovery code = unrecoverable data.** No server-side reset.
6. **Errors surface verbatim; nothing auto-routes or falls back.**

### 1.2 What a user can do today

| Capability | Status | Where |
|---|---|---|
| Create an account on a relay (email + password), forced recovery-code capture | Shipped | Setup wizard, `sync_signup` |
| Point the app at a self-hosted relay (typed probe, advisory version gap, "use anyway") | Shipped | Wizard server step, `sync_probe` |
| Sign a second device in with a **proof of possession** of its device key; it waits as **pending** until an existing device approves it | Shipped | Wizard account + approval steps, `sync_login_*`, CRYPTO_SPEC A10 |
| Approve a device by comparing the device fingerprint and the roster **security code** on both screens | Shipped | `DeviceApprovalDialog`, `sync_approve_device` |
| Sign in with the recovery code when the password is lost; a new device admits itself to the roster (no approver needed) | Shipped | `sync_recover`, CRYPTO_SPEC A7 |
| Turn sync on: the live database becomes CRDT-tracked, history is captured, the drain starts | Shipped | Wizard turn-on step, `sync_enable` |
| Steady-state sync of sessions, transcripts, notes, folders, tags, chat, dictation, audio-part rows | Shipped | Drain, §6 |
| Follow a session that is **recording on another device** live (read-only) | Shipped | `NoteDetailView` remote-live branch, §8 |
| Audio backup: new recordings and dictations upload in the background; the existing library backfills | Shipped | Audio lane, §7 |
| Play audio recorded elsewhere: fetch on demand with progress, cancel, disk precheck | Shipped | `audio_prepare_part`, §7.4 |
| See status everywhere: settings badge and line, sidebar glyph, device tiles, toasts | Shipped | §10 |
| See and retry **unreadable** (undecryptable) peer changes | Shipped | `UnreadableChangesWarning`, `sync_retry_crypto_quarantine` |
| Clear downloaded (fetched) audio | Shipped | `audio_cache_clear` |
| Sign out (keeps the account anchor so re-sign-in is not TOFU); change server (= sign out) | Shipped | `sync_sign_out` |
| Forget an account on this device (clears the anchor, so the next sign-in is trust-on-first-use again) | Command only, no UI (nothing in the app invokes it) | `sync_forget_account` |
| Turn sync **off** while staying signed in (and back on, resuming in place) | Shipped | `sync_disable`, Sync page "Turn off" |
| Change password (from Account, or offered right after a recovery sign-in) | Shipped | `sync_change_password`, CRYPTO_SPEC §4.3 + A9 |
| Remove another device from the account (it loses relay access; rejoining needs approval) | Shipped | `sync_remove_device`, tile "Remove" |
| Regenerate the recovery code; rotate the vault key | **Not built** | — |
| Sync settings or AI connection keys across devices | **Not built**; not planned (§4.5, §15.H) | — |
| Share a note by private link | **Not built**; tables only | — |
| Bootstrap a large library from a snapshot instead of replaying history | **Not available in the app**; server routes + engine seam exist (§14 D10), design proposed (§15.F) | — |
| Keep syncing from an app build older than the relay's protocol version 2 | **Refused**: the sign-in succeeds but binds no device, every write is rejected and the app must be updated (§11, A10) | — |
| Use a hosted relay | **None exists**; default server URL is empty | `DEFAULT_SYNC_SERVER_URL` |

### 1.3 What the relay can and cannot see

Unchanged from README and `self-hosting.md`, restated because the spec must be
self-contained:

- **Never:** note bodies, segments, sessions, folders, chat, dictation text, audio
  bytes, AI prompts, the password, any unwrapped key.
- **Yes:** email, auth verifier (a second hash), KDF salt, wrapped vault keys (inert),
  the signed device roster as an opaque blob plus its integer counter and epoch, device
  labels, platforms and states (pending/active/revoked), which device authored each
  changeset and its per-device sequence number, changeset count/size/timing, audio blob
  hashes and sizes with the part and session ids they belong to, sign-in challenges
  (random nonces bound to an email for five minutes), IP.
- **Can do:** deny service (withhold, delay, delete). Cannot decrypt, forge an
  approval, or alter content without the change failing authentication and landing in
  quarantine.

---

## 2. Build and deployment surface

| Item | Value |
|---|---|
| Desktop cargo feature | `sync` (off by default in a plain `tauri dev`; `pnpm check`'s `--all` does not enable it) |
| Release workflow | `release.yml` builds both platforms with `-f sync`; no tagged release has shipped from this branch yet |
| On-demand test builds | `sync-test-build.yml` |
| Feature-absent behaviour | Every `sync_*` command is unregistered; the frontend detects `Command sync_status not found` by full string equality and shows a "Not in this build" placeholder, no polling |
| Relay deploy | `deploy/docker-compose.yml`: `postgres:16`, `minio`, one-shot `minio-init`, one-shot `migrate` (owner DSN), `server` (serves as non-owner `yapstack_app`) |
| Relay config | `yapstack-relay.toml` rendered by `entrypoint.sh` from `.env`; `[limits]` absent on self-host |
| Client gate | `pnpm check`, `cargo clippy --workspace --all-targets --features yapstack-desktop/sync --tests -- -D warnings`, `cargo test -p yapstack-desktop --features sync` |
| Server live tests | `DATABASE_URL=… cargo test -p yapstack-server -- --ignored` against a real Postgres (MinIO optional) |

---

## 3. Components

```
apps/desktop/src/…              React front end
  lib/sync.ts                     IPC contract + pure display helpers (hand-typed, feature-independent)
  lib/syncDisplay.ts              deriveSyncDisplay — the one status derivation
  components/sync/SyncTab.tsx     Settings → Sync page (hero when off, devices page when on)
  components/sync/SyncSetupWizard.tsx  guided setup (server → account → approval → turn on)
  components/sync/DeviceApprovalDialog.tsx  approver's ceremony
  components/SyncStatusGlyph.tsx  sidebar glyph + app-wide poll + approval toast
  stores/appStore.ts              syncConfig (persisted, no secrets), syncStatus, relayConn, sync://applied refresh
apps/desktop/src-tauri/src/sync.rs   desktop integration: credential store, ceremonies, cutover, drain lanes, commands
crates/yapstack-sync              engine: CRR schema migration, outbox, quarantine, cascade/uniqueness, audio queue + fetch, transport
crates/yapstack-crypto            RustCrypto implementation of CRYPTO_SPEC (+ KATs)
crates/yapstack-common            wire DTOs shared by client and relay (auth.rs, sync.rs)
crates/yapstack-entitlements      Limit types, AllowAll / StoredLimits, admin wire types
crates/yapstack-server            the blind relay (axum + sqlx + Postgres, S3 presigning)
deploy/                           compose stack, Dockerfile, entrypoint, postgres-init
```

The vendored cr-sqlite v0.16.3 is statically linked by `crates/yapstack-sync/build.rs`
and initialised **per connection** (never `sqlite3_auto_extension`), so the app's
ordinary connections are unaffected.

---

## 4. Data model

### 4.1 Synced tables

Exactly these twelve tables are converted to cr-sqlite CRRs at cutover
(`schema::SYNC_TABLES`): `sessions`, `segments`, `notes`, `note_versions`, `folders`,
`session_folders`, `chat_messages`, `dictation_history`, `tags`, `session_tags`,
`session_audio_parts`, `shares`. Every one has a TEXT UUID primary key (two composite
UUID junctions). Everything else (`*_fts*`, embeddings, `pending_audio_deletions`,
`_sqlx_migrations`, `sqlite_sequence`, and every `_yapstack_*` table) is local-only.

The CRR rebuild (`schema::derive_rebuild_body`) strips FK constraints, non-PK UNIQUE
indexes and CHECKs, and gives NOT-NULL-no-default columns a synthetic default. Two
app-layer passes reinstate what was stripped after each converged merge:

- `cascade::cascade_gc` — ON DELETE CASCADE / SET NULL, keyed on **proven** parent
  deletes (clock-table sentinel), never on mere absence.
- `uniqueness::enforce_uniqueness` — the three dropped UNIQUEs (`notes.session_id`,
  `tags.name` NOCASE, `session_audio_parts(session_id, part_index)`), resolved to a
  deterministic winner from **proven-merged** columns; losers tombstone through the CRDT.

### 4.2 Local sync bookkeeping (never synced)

| Table | Holds |
|---|---|
| `_yapstack_sync_meta` | `client_id` (unused in production; the keychain identity wins), `pull_watermark`, `push_watermark`, `client_seq`, `seq_reconciled`, `bound_tenant_id` |
| `_yapstack_sync_prep` | `schema_version` marker that the live DB has been CRR-prepared |
| `_yapstack_outbox` | encrypted, chunked local changesets awaiting ack |
| `_yapstack_pending_changes` | schema-gap quarantine (decrypted rows for unknown columns, clock intact) |
| `_yapstack_crypto_quarantine` | undecryptable pulled changesets (ciphertext, durable, retryable) |
| `_yapstack_audio_upload_queue` | audio upload state machine per part |

### 4.3 Schema evolution on a CRR database

Migrations (`db.rs`) carry a `kind` (ADR 0006 §1). On a CRR database the runner
(`db_service::apply_migration`) treats them as follows; on a non-CRR database every kind
is applied plainly.

- **`Plain`**: a single `ALTER TABLE <sync_table>` is routed through `crsql_alter`;
  any other statement that names a sync table is refused.
- **`SyncIndex`**: only `CREATE [UNIQUE] INDEX IF NOT EXISTS … ON <sync_table>(…)`,
  applied plainly (an index touches no rows, columns or clocks, so CRR triggers stay
  current). A UNIQUE index whose key is not exactly the table's PRIMARY KEY is refused
  on every database, so a fresh DB never holds an index cutover would drop.

**Indexes survive cutover.** The rebuild (`schema::transform_and_crrify`) captures each
table's declared non-UNIQUE indexes from `sqlite_master` (never `sqlite_autoindex_*`,
never UNIQUE) and replays them after the RENAME, alongside the app triggers. Builds
before this dropped every secondary index at cutover; migration v18 (`SyncIndex`)
re-declares every synced-table secondary index with `IF NOT EXISTS` to restore them on
those databases (I-NT2). A new index on a synced table is a `SyncIndex` migration.

The rebuild does not carry a column `COLLATE` over, so `tags.name` (and with it
`idx_tags_name`) compares BINARY after cutover rather than NOCASE; the NOCASE rule
itself is enforced by `uniqueness`.

#### Column additions that must be CRR-tracked

`schema::OUT_OF_BAND_ALTERS` lists every column the front end historically added at
runtime (`segments.speaker_id`, `sessions.recording_device_id`, six `chat_messages`
columns). At cutover and at every drain start they are applied through the
`crsql_alter` dance, and `heal_stale_crr_triggers` repairs tables whose triggers were
left at an old arity by a bare ALTER. The front end's `addColumnIfMissing` refuses to
bare-ALTER a CRR-tracked table and defers to this path.

### 4.4 Attribution column

`sessions.recording_device_id` (synced, nullable) = the recorder's device fingerprint.
It drives the remote-live view (§8) and the ownership-gated boot sweep in `db.rs`.

### 4.5 Settings

No setting travels over sync. The per-field classification below is the design record
(formerly the compile-checked `SETTINGS_SYNC_POLICY` table, deleted in the
simplification pass because nothing consumed it). If settings sync is ever built, it
starts from this list and secrets are vault-wrapped before any persistence that could
sync.

| Class | Fields |
|---|---|
| device-local, never synced | `selectedMicDeviceId`, `captureSource`, `selectedEngine`, `selectedModelSize`, `selectedParakeetVariant`, `diarizationEnabled`, `sidebarCollapsed`, `theme`, `showRecordingIndicator`, `analyticsEnabled`, `dictationDuckEnabled`, `dictationDuckAmount`, `audioSaveLocation`, `onboarding` |
| synced (plaintext-safe preferences) | `speakerNames`, `language`, `mixConfig`, `bufferMaxSeconds`, `silenceDurationMs`, `maxChunkSeconds`, `promptContextChars`, `promptDecaySilenceSeconds`, `shortcutBindings`, `audioExportFormat`, `mp3Bitrate`, `dictation`, `insights` |
| secret (must be vault-wrapped, never synced raw) | `aiConfig` (connection `apiKey` / `baseUrl`) |

### 4.6 Audio

Audio is not in the changeset stream. Each `session_audio_parts` row (and each
`dictation_history` row, which is its own part) maps to at most one encrypted blob in
object storage, addressed by the **part row UUID**, content-addressed on the ciphertext
SHA-256, and de-duplicated within a tenant with a mapping-count refcount (§7).

---

## 5. Identity, account and device lifecycle

### 5.1 Stores on the device

Three logical entries under keychain service `dev.yapstack.app.sync`:

| Entry | Survives sign-out | Contents |
|---|---|---|
| `identity-v1` | yes | `client_id` (UUIDv4 minted once per install) + Ed25519 signing seed |
| `account-anchor-v1:<sha256(server_url,email)>` | yes (cleared only by `sync_forget_account`) | tenant id, epoch, first-seen `salt_enc`, highest verified roster counter, roster fingerprint |
| session | no | server URL, email, vault key, epoch, tenant, access + refresh tokens, device fingerprint, `sync_enabled`, roster counter/fingerprint |

**Release builds** seal the session JSON under a random 32-byte wrapping key held in the
keychain (`session-key-v1`) and write the ciphertext to `sync-session.enc` (owner-only
mode on unix), AAD domain `yapstack.session.store.v2` binding the epoch. This exists
because Windows Credential Manager cannot hold the session blob. **Debug builds** use a
plaintext dev file and make zero keychain calls (macOS re-prompts on every rebuilt
binary). CRYPTO_SPEC §10 describes only the keychain; §14 D15 records this amendment.

An in-process cache fronts the store so the 1–5 s status poll never re-reads the
keychain. Session writes are field-merged under a lock (`update_session`) so a
command that captured the session before a network round-trip cannot clobber a
refresh token the drain rotated meanwhile.

### 5.2 Signup (first device)

`sync_signup(server_url, email, password)`:

1. Fresh 16-byte `salt_enc`; Argon2id stretch; HKDF split → `auth_key`, `master_key`
   (CRYPTO_SPEC §2).
2. Fresh 32-byte vault key; fresh 20-byte recovery code → `recovery_key`,
   `recovery_auth_key` (§6.2).
3. Committing wraps of the vault key under `master_key` and `recovery_key` (§4.2).
4. Load-or-create the device identity; self-sign the epoch-0, counter-0 roster with
   `tenant_id = nil` (§7.5, accepted).
5. `POST /auth/signup` with `auth_key`, `recovery_auth_key`, salt, both wraps, the
   roster envelope (which also carries `label` and `platform` for the device index).
6. Store the session (`sync_enabled = false`) and the account anchor.
7. Return the recovery code **once**; the wizard forces copy, acknowledgement, and
   typed confirmation before it can be dismissed.

### 5.3 Login (additional or returning device)

Two rounds. `sync_login_begin(server_url, email)` fetches `salt_enc`; a known device
(anchor or session baseline present) compares it and reports `saltMismatch`, on which
the wizard **stops** ("Stopped for your safety") and never collects the password. A
first-time device accepts the served salt (TOFU, residual §3.3).

`sync_login_finish(password)` derives `auth_key`, sends this device's `client_id`,
Ed25519 public key, label and platform **plus a proof of possession** — the
`login/begin` nonce signed with the device key (CRYPTO_SPEC A10) — receives tokens +
the password wrap + the signed roster, unwraps the vault key, **verifies the roster
signature and the counter against the cached floor**, and stores the session with
`sync_enabled = false`. The relay verifies the proof against the stored key for a known
`client_id`, enrols an unknown device as `pending`, and leaves a known device untouched;
without a valid proof the token binds no device and every write is refused. The client
fails closed if the relay issues no nonce, refreshes a nonce older than 4 minutes
transparently (re-checking the salt), and retries a 401 once before reporting
"Incorrect password".

### 5.4 Approval ceremony

- The waiting device polls status every 3 s and shows its own fingerprint plus
  "Security code XXXX-XXXX-XXXX-XXXX · key vN" (roster fingerprint + epoch, §7.5.4).
- Every other signed-in device sees an amber "Approve new device" state, a persistent
  toast, and an amber tile with **Review**. The approver's dialog shows the same code
  and requires the checkbox "I compared the codes on both devices and they match."
- `sync_approve_device(fingerprint)`: fetch the authoritative roster (`GET
  /devices/roster`), verify it, hard-fail if the relay's plaintext counter disagrees
  with the signed one, ratchet and persist the floor **before** mutating, discover the
  pending target by fingerprint from `GET /devices` (never as the membership source),
  carry the verified membership forward, hard-fail on any pubkey substitution, sign
  `counter + 1` at the current epoch, `PUT /devices/roster` with `active_devices`;
  retry once on 409.
- The waiting device notices `!isSelfPending` on its next poll and advances.

### 5.5 Recovery

`sync_recover(server_url, email, code)` fetches a nonce from `login/begin`, sends this
device's identity and proof of possession (A10) with the recovery auth key, receives the recovery wrap, unwraps the vault key and verifies the
served roster. If this device is **not a roster member** it admits itself (CRYPTO_SPEC
A7): the verified membership is carried forward, this device is appended, the counter
advances by exactly one at the same epoch, the roster is re-signed and uploaded. The
relay accepts that single upload from a still-pending caller because the recovery login
stamped a one-shot, 15-minute `recovery_grant_at` on its device row (only on a row of
the same account; it cannot promote a third party); the device is then active, the
security code changes, and any surviving device sees an ordinary counter advance. The
session is stored before the upload, so a 409 (a concurrent approval moved the counter)
leaves the user signed in and pending with an explicit message. A recovering device
that holds an anchor for the account refuses a served roster whose fingerprint differs
from the anchored one at the same counter; a brand-new install is TOFU-grade here
(CRYPTO_SPEC A7 step 3). The forgotten password stays in force until password change
ships (§15.C). Against an older relay that does not enrol on recovery, the client skips
the upload and `sync_enable` refuses the unenrolled session with an explicit message.

### 5.6 Password change

`sync_change_password(current_password?, new_password)` stretches the new password
under the account's **existing** `salt_enc` (CRYPTO_SPEC A9, so every known device's
salt baseline stays valid), re-wraps the vault key under the new master key, and calls
`PUT /auth/password`. The relay requires an active device and either the current
`auth_key` or a recovery-code sign-in on that device within the last 15 minutes (a
single-use waiver, consumed atomically), then atomically replaces the verifier (fresh
server salt) and the password wrap **and revokes every other device's refresh
families** — a device enrolled with the leaked password loses relay access, and every
legitimate device shows "Sign in again" and re-authenticates with the new password (its
vault key and local data are untouched). Nothing changes on the calling device. The
client refreshes its token once on a 401 before attributing the failure to the
credential, and maps 403 to "approve this device first". Offered in the wizard
immediately after a recovery sign-in ("Set a new password", skippable) and from the
Account row ("Change password", asks for the current password).

### 5.7 Device removal

`sync_remove_device(fingerprint, password)` (never this device): re-anchor on the
authoritative signed roster, drop the target from the **verified** membership, sign
`counter + 1` at the current epoch, and `PUT /devices/roster` with `active_devices` =
the remaining members, `revoked_devices = [target]`, and the caller's current
`auth_key` (re-authentication — a stolen access token alone cannot evict devices). The
relay marks the row `revoked`, clears any grants on it, and revokes its refresh
families; `require_active_caller` refuses its writes immediately (push, audio presign,
snapshot presign, roster) and its reads end with its access token (≤ 15 min). Only an
active caller may revoke (never on the recovery re-bootstrap path, never itself, at most
32 per upload, and an id cannot be both revoked and active). A revoked device that signs
in again with the password, or recovers with the code, is re-enrolled **pending with no
recovery grant** and must be approved anew. Removed peers disappear from the device
tiles; a removed device sees "This device was removed" with Sign out (hero when sync is
off, banner when on) and `sync_enable` refuses it.

**What removal guarantees, honestly.** It is relay-side eviction of that device's
*tokens*. The removed device keeps what it already synced (retiring its key is vault-key
rotation, CRYPTO_SPEC §4.4, deferred). If whoever holds it knows the **password**, they
can sign in again and — because pending devices may pull — keep *reading* new changes
until the password is changed (§5.6), which the Remove dialog says. A **compromised
relay** can undo removal by flipping the row back (peers do not filter pulled changes by
roster membership). Since A10, a password holder can no longer mint a token for another device's
`client_id` (§15.K).

### 5.8 Tokens

Access JWT 15 min, refresh JWT 30 days, per-device families with rotation and reuse
detection (a spent token presented again revokes the family, serialised with `FOR
UPDATE`). The drain refreshes once on a 401 and proactively when `exp` is within 60 s,
persisting the rotated pair **before** using it. A rejected refresh strips the
credentials but keeps the session record, so the UI lands on "Sign in again" rather
than "off". The audio lane never refreshes; it reloads the persisted bearer.

### 5.9 Sign-out, change server, forget

- **Turn off sync** (`sync_disable`): stops all lanes off the event loop, clears their
  published state, persists `sync_enabled = false`; the session, tokens, vault key and
  roster are untouched and the database stays CRR-prepared with its watermarks and
  account binding, so turning sync back on resumes in place. The Sync page then shows
  the hero with "Turn on sync".
- **Sign out**: stops all lanes off the event loop, deletes the session entry, keeps
  the identity and the account anchor, leaves the database CRR-prepared and its
  watermarks in place (bound to the tenant).
- **Change server** is sign-out with a warning.
- **Forget account**: clears the anchor; the next sign-in is TOFU again. No UI.

---

## 6. Enable and the changeset drain

### 6.1 Enable (`sync_enable`)

1. Refuse if this device is still pending.
2. **Cutover** (`perform_cutover`): idempotent on an already-prepared DB; otherwise
   `VACUUM INTO` a staging copy, `crr_migrate` it, apply out-of-band ALTERs, heal
   triggers, run cascade + uniqueness once, zero the watermarks, checkpoint, then swap
   files under a durable journal with a `.backup` kept and a row-count oracle across
   **all** user tables; any mismatch rolls back. `recover_interrupted_cutover` at boot
   completes or reverts a crashed swap before the database is opened.
3. **Account binding** (`reconcile_account_binding`): if `bound_tenant_id` differs
   from the session tenant, zero both watermarks and empty the outbox, then record the
   binding. Runs outside the cutover idempotency gate at enable and at boot.
4. Spawn the drain, then stop any previous drain on a blocking task.
5. Persist `sync_enabled = true` and refresh the anchor.

There is no reverse path: once prepared, a database stays CRR-prepared. Turning sync
off (`sync_disable`) stops the lanes and clears `sync_enabled`; it does not touch the
database.

### 6.2 Lanes

`spawn_drain` starts four OS threads sharing one session tuple:

| Lane | Thread | Cadence | Role |
|---|---|---|---|
| Drain | `yapstack-sync-drain` (current-thread tokio) | 5 s floor, plus wakes | push then pull, every cycle, both directions always attempted |
| SSE subscriber | own thread | reconnect backoff 1 s → 30 s | `GET /sync/stream` wakeups; wakes the drain only when the advertised seq exceeds the local pull watermark |
| Write-kick debouncer | own thread | 400 ms trailing | `note_local_write()` after each committed UI write wakes the drain |
| Audio uploader | `yapstack-audio-uploader` | 8 s idle interval | §7.2 |

### 6.3 Push

`enqueue_local` reads `crsql_changes` since the push watermark (own site only), plans
**row-atomic** chunks (5 000 rows or 512 KiB plaintext, never splitting a `(table,
pk)` group), encrypts each chunk with a fresh data key wrapped under the vault key
(CRYPTO_SPEC §4/§5, AAD binds tenant, client, `client_seq`, schema and engine
versions), and stages it in the outbox. `push_outbox` ships batches under a 1.5 MB
wire budget; an entry that can never fit surfaces once as `Blocked` rather than
hot-looping. `(client_id, client_seq)` is the idempotency key; the relay returns dense
commit-ordered `changeset_seq`s and rejects a reused `client_seq` with **different**
content (counter regression after a restore) with 409, on which the client reseeds its
counter from the relay's per-client tail.

### 6.4 Pull

`pull_direction` pages `GET /sync/pull` (500 per page), merges **every** changeset,
including its own echoes (idempotent; this is how a restored database re-acquires its
own history), one transaction per changeset with cooperative yields and a real pause
every 32 merges so UI writers can take the lock. A changeset that fails to decrypt or
decode is stored in the crypto quarantine and the watermark **advances past it**
(§11.3 as amended: durable, surfaced, retryable, never wedging). Unknown-column rows go
to the schema quarantine and replay after the column lands. The watermark is written
once per page.

### 6.5 Maintenance

`cascade_gc` and `enforce_uniqueness` run only on a cycle that merged something **and**
completed a clean, fully-drained pull (`maintenance_ready`); a merge on a failed pull
arms a flag consumed by the next clean cycle. Rationale: proven ≠ fresh.

### 6.6 Health and progress

Two one-way cells feed `sync_status`:

- `DrainHealth`: `Ok` | `AuthExpired` | `Blocked(msg)` | `Failing(msg)` |
  `Unreachable(msg)`. Transport errors surface only after **2 consecutive** failing
  cycles; a clean cycle clears. Connectivity failures are typed at the transport
  (`SyncError::Network`), never string-matched.
- `DrainProgress`: pending entries/bytes, acked this session, `syncing`,
  `catching_up` + `pull_behind` (from the last-known relay tip, seeded by one
  completeness call at spawn), `last_success`, `crypto_quarantined` (durable count).

After a cycle that applied peer rows the drain emits `sync://applied`; the store
debounces and refreshes lists and the open session (§8), skipping while a segment or
note edit is in progress and draining the skip afterwards.

### 6.7 Status DTO

`SyncStatusDto` (`lib/sync.ts` `SyncStatus`): `phase` ∈ `disconnected | connecting |
connected | syncing | catching_up | auth_expired | unreachable | error`, server URL,
email, own fingerprint, roster (fingerprint, isSelf, pending, label, platform per
device from `GET /devices`), epoch, roster fingerprint, `syncEnabled`, `lastError`,
push progress (`pendingEntries`, `pendingBytes`, `ackedThisSession`, `lastSuccess`),
`pullBehind`, `cryptoQuarantined`, five audio-lane counters, and `removed` (§5.7: this
device was removed from the account by another device). Field-level shapes are in
[`API_REFERENCE.md`](API_REFERENCE.md#sync-yapstack-desktop-yapstack-common).

---

## 7. Audio lane

### 7.1 Blob format and identity

CRYPTO_SPEC §1.5 STREAM envelope; the per-blob data key wrap binds `(tenant_id,
session_id, part_id, epoch)`. Dictation audio uses `part_id = session_id =
dictation_history.id`.

### 7.2 Upload

- Producers: session finalize (`enqueueAudioForSession`, fire-and-forget) and
  dictation save (`enqueueAudioForDictation`) → NORMAL priority; the idempotent
  **backfill walk** at every uploader start → LOW priority, deferred up to 30 s for
  the first drain cycle, stepping over locked parts and re-running next start until a
  pass covers every local part.
- Uploader: one blob at a time, seal to a temp file beside the DB, content-address,
  `POST /audio/presign` (existence-checked; a dead PUT yields a fresh URL, never a
  phantom done), streaming PUT with pinned length. States `pending → sealing →
  uploading → done | failed`; in-flight rows re-arm and failed rows retry at start and
  via **Retry** in settings. Deleted parts drop silently.
- The relay: within-tenant dedup on ciphertext hash, mapping-count refcount, a
  `released_at` stamp when refcount hits 0, and a GC sweep (default every 24 h,
  7-day grace, storage object deleted **before** the row, then
  `tenant_usage.storage_bytes` decremented in the same transaction, floored at 0).

### 7.3 Server completeness invariant (owner)

Every device treats audio that exists locally but not on the relay as outstanding
upload debt; the relay should hold the full history. Enforced by finalize-enqueue plus
the backfill walk on every device that ever recorded. No periodic audit yet.

### 7.4 Fetch on demand

Resolution order per part: local fetch cache (beside the DB, keyed by part id) → the
synced `file_path` **only** if it resolves under this device's audio directory → fetch.
`audio_prepare_part` is polled by the player: it coalesces onto a single in-flight
download per part, admits at most 2 concurrent downloads (FIFO, session views jump the
queue at HIGH), prechecks disk space (256 MiB headroom), streams to a temp file,
decrypts segment-by-segment verifying identity at unwrap, and promotes to the cache.
A cache hit or a completed fetch registers the fetch-cache directory with the
`audio-stream://` allow-list (`register_trusted_audio_dir`), which is how fetched audio
becomes playable through the same protocol as locally recorded audio.
Terminal states are distinct and honest: `not_on_server` (re-probed every 30 s, the
fetch self-starts when the source device uploads), `unreachable`, `auth_expired`
(self-healing via the drain's refresh), `no_space`, `verification_failed` (sticky
until explicit retry), `error`. `audio_release_part` drops a still-queued part on
navigate-away; an in-flight download completes into the cache. The cache is
keep-until-manual-clear (`audio_cache_clear`, skips in-flight parts).

---

## 8. Live sessions across devices

A session row with `status='recording'` and a foreign `recording_device_id` renders as
a **read-only follow-along** ("● Live on <label>") whose transcript, parts and note
fill as changesets merge. Liveness is the heartbeat `max(segments.created_at)`
(fallback `created_at`) with a 3-minute threshold; stale → "Interrupted on <label>",
still read-only. Finalisation authority is owner-only: the recorder's boot sweep
finalises its own crashed rows; a foreign row is never finalised or deleted by another
device. A permanently dead device leaves an interrupted row, reclaimable via the manual
"mark completed" action. The resume race is guarded by the existing `status !==
'completed'` refusal.

---

## 9. Relay server

### 9.1 Endpoints

| Route | Auth | Purpose |
|---|---|---|
| `GET /health`, `GET /version` | none | liveness, build digest |
| `GET /sync/info` | none | `protocol_version`, `min_client_version`, `engine_version`, optional `billing_url` — the probe sentinel |
| `POST /auth/signup` | none; per-IP 5/min; optional `X-YapStack-Invite` | create user + workspace + first device + roster |
| `POST /auth/login/begin` | none; per-IP 120/min | `salt_enc` or a peppered decoy, plus a single-use 5-minute proof-of-possession nonce (A10) |
| `POST /auth/login/finish` | none; per-IP 10/min | verifier check; device identity honoured only with a valid nonce + device-key proof verified against the stored key (A10); enrol unknown device pending; tokens + wraps + roster |
| `POST /auth/refresh` | refresh JWT | rotation + reuse detection |
| `PUT /auth/password` | access JWT of an **active** device; per-(tenant, IP) 120/min; current `auth_key` or a recovery sign-in within 15 min | replaces verifier + password wrap, keeps `salt_enc` (A9) |
| `POST /auth/recover` | none; per-IP | recovery verifier check; device identity honoured only with a valid proof (A10); enrols the presented device pending with a one-shot, 15-min `recovery_grant_at` (same-account rows only, else 409; none for a revoked row); recovery wrap + roster; tokens bound to the device |
| `GET /devices` | access JWT | device index (client_id, pubkey, label, platform, status, added_at) |
| `GET /devices/roster` | access JWT (pending allowed) | stored signed roster + watermarks |
| `PUT /devices/roster` | access JWT with `client_id` whose row is **active** (plus the current `auth_key` when `revoked_devices` is non-empty), or pending with a fresh `recovery_grant_at` (then exactly counter+1, same epoch, names itself, promotes nobody else) | counter strictly increases, epoch never decreases, 64-byte signature present; promotes `active_devices`; revokes `revoked_devices` (active callers only, never self); consumes the grant |
| `POST /sync/push` | access JWT of an active device; per-(tenant, IP) 120/min | ≤1000 changes, ≤5 MiB raw; dense commit-ordered seqs from `tenant_seq` under `FOR UPDATE`; idempotent; 409 on content-differing reuse; choke-metered |
| `GET /sync/pull?since&limit` | access JWT | dense pages, never gated |
| `GET /sync/completeness` | access JWT | `max`, `count`, `contiguous`, per-client tails |
| `GET /sync/stream` | access JWT | SSE `wakeup` events carrying only the seq |
| `POST /audio/presign?sha256&size&part_id&session_id` | access JWT | dedup + refcount + existence-checked presigned PUT |
| `GET /audio/part/{part_id}` | access JWT | 302 to a presigned GET, never gated |
| `POST /snapshot/presign`, `GET /snapshot` | access JWT (presign: **active** device) | snapshot bootstrap seam (no client caller) |
| `PUT /admin/v1/tenants/{id}/limits`, `GET …/usage` | Ed25519-signed control-plane requests | mounted only when `[limits].admin_public_key` is set |

Every device-authored write is gated on the caller's device row: push, audio presign,
snapshot presign and `PUT /auth/password` through `devices::require_active_caller`
(active only), and `PUT /devices/roster` through its own gate in `put_roster` (active,
or pending with a fresh recovery grant — A7; removal additionally re-authenticates).
Pull, audio GET, the roster GET, completeness and the SSE stream are never gated. An
access token outlives a revocation by at most its 15-minute TTL (reads only; writes are
refused immediately).

### 9.2 Schema (migrations 0001–0013)

`workspaces`, `users` (verifier, salts, both wraps, recovery verifier), `workspace_members`,
`devices` (pubkey, label, platform, `status` pending|active|revoked, one-shot `recovery_grant_at`, single-use `recovered_at`, unused `state`
active|stale|retired, `last_seen` never updated), `device_roster` (opaque list +
signature + counter + epoch), `refresh_tokens`, `tenant_limits`, `tenant_usage`,
`tenant_seq`, `changesets`, `shares` (unused), `login_nonces` (A10, non-RLS), `audio_blobs` (+ `released_at`),
`audio_objects` keyed by part, `snapshots`, `admin_nonces`.

Migrations 0010–0013 are the feature slices' additions, all additive: `0010`
`devices.recovery_grant_at` (A7), `0011` `devices.recovered_at` (A9 waiver), `0012`
widens the `devices.status` CHECK to admit `revoked` (§15.D), `0013` creates
`login_nonces` and revokes every device-bound refresh family minted before A10 (so each
device signs in again with proof). `0010`/`0011` use `ADD COLUMN IF NOT EXISTS`;
re-run safety otherwise comes from sqlx's `_sqlx_migrations` ledger, as for every earlier
migration. The one-shot `migrate` service runs them as the database owner; the
`refresh_tokens` revocation in `0013` is a plain `UPDATE` on a FORCE-RLS table and only
takes effect when that owner bypasses RLS (a superuser or a `BYPASSRLS` role — the
compose default; see the managed-Postgres caveat in `self-hosting.md`). Operator steps
are in [`self-hosting.md`](self-hosting.md#upgrading).

### 9.3 Isolation

Every tenant table is `ENABLE` + `FORCE ROW LEVEL SECURITY` on a transaction-local
`app.tenant_id` set only from the validated JWT; the predicate fails closed. The relay
serves as the non-owner `yapstack_app` role; migrations run as the owner in a one-shot
container (`YAPSTACK_MIGRATE_ONLY`), which also sets the app role's password.
Pre-tenant login/recover resolve the workspace through the `SECURITY DEFINER`
`yapstack_lookup_login`. A CI test scans every stored byte for a plaintext canary.

### 9.4 Operational knobs

`YAPSTACK_TRUSTED_PROXIES` (XFF honoured only from listed peers, fail-closed),
`YAPSTACK_SIGNUP_INVITE`, `YAPSTACK_GC_*`, `MINIO_PUBLIC_ENDPOINT`, rate limits in
`[ratelimit]`, `[storage]` (absent ⇒ audio and snapshot presign disabled), `[limits]`
(absent ⇒ AllowAll, admin API unmounted). Argon2 verifier hashing runs on the blocking
pool under a semaphore.

---

## 10. Front-end surface

### 10.1 Settings → Sync

- **Sync off / half-set-up:** a single hero (device illustration, one sentence,
  "Set up sync" or "Continue setup", "Use a different account" when signed in but not
  enabled, "Waiting for approval" copy when pending).
- **Sync on:** heading with `StatusBadge` + `SyncStatusLine`; a "Connection problem"
  alert with the verbatim error (not when auth expired); an amber "Sign in again" row
  when expired; `UnreadableChangesWarning` with Retry; the **Your devices** grid
  (pending tiles with Review, device tiles with OS icon and "This device", an "Add a
  device" tile opening a guide that shows this account's server and email, and **Remove**
  on every other device's tile — confirmation asks for the password and says what removal
  does and does not guarantee, §5.7); audio backup row; downloaded-audio row with Clear;
  "Sync on this device" row with **Turn off** (confirmation; §5.9); Server row (host only,
  Change = sign out); Account row (**Change password**, Sign out).
- **Removed device:** "This device was removed" with Sign out — a banner when sync is on,
  the hero when it is off; Remove is hidden and `sync_enable` refuses (§5.7).
- Deep links: `settingsRequest` `"sync"` (glyph, toast) and `"sync-setup"` (onboarding,
  hero) open the tab and, for the latter, the wizard.

### 10.2 Setup wizard

Steps `intro → server → account → approval → turn-on → done`, resuming at the right
step from saved state. The server step probes on blur/paste/Enter, shows the typed
failure with "Use this address anyway", and treats a version gap as advisory. The
account step is a tab pair (Sign in / Create account) with the recovery-code ceremony
locking the dialog until acknowledged. Codes appear **only** in the approval step and
the approver's dialog.

### 10.3 One status derivation

`deriveSyncDisplay({ conn, status, signedIn, syncEnabled, syncingSince? })` precedence
(`SyncTab` feeds `signedIn`/`syncEnabled` from the live status DTO, the sidebar glyph from
the persisted `syncConfig`; neither caller passes `syncingSince` today, so the 2 s motion
gate is exercised only by its unit test):

`off (signed out)` → `waiting-approval` → `approval-needed` → `off (not enabled)` →
`unreachable` (probe failure or drain `unreachable`) → `auth-expired` → `error`
(phase error or any `lastError`) → `syncing` (motion only past 3 entries or 2 s) →
`catching-up` (phase or `pullBehind > 0`) → `pending` → `quarantined` (amber "Synced —
N changes unreadable") → `caught-up` ("Up to date · synced Xm ago"). The `removed` flag
is rendered by the Sync page alone (§10.1); the derivation does not consume it.

The sidebar glyph, badge and status line all render this object. The glyph polls every
5 s (2 s while syncing or catching up) once the user has ever signed in or enabled
sync, and raises a persistent "A new device is requesting access" toast per pending
peer count change.

### 10.4 First-run onboarding

An optional step "Use YapStack on more than one device?" hands off to the wizard after
onboarding completes.

---

## 11. Versioning and evolution

- **Additive-only schema changes between releases** (decision report §6.2). A new
  synced column must be added in `db.rs` migrations (wrapped through `crsql_alter` on a
  CRR DB by `run_migrations`) **and** registered in `OUT_OF_BAND_ALTERS`.
- `SYNC_SCHEMA_VERSION` (1) and `CRSQLITE_ENGINE_VERSION` (16003) are bound into every
  changeset AAD; the relay's plaintext copies are untrusted hints. Because the AAD
  binds the **local** versions, any version skew between peers quarantines wholesale
  until the lagging device updates and retries — this is the v1 posture (§14 D9).
- Relay `min_client_version` is advisory ("update this app"); there is no per-tenant
  floor and no device staleness lifecycle (§14 D5).
- Relay `protocol_version` is **2** since A10 (the login challenge). The client checks
  only that the field is present (the not-a-relay sentinel); the version itself is
  informational. Compatibility is enforced by behaviour: a client older than A10 signs in
  without a proof, binds no device and has every write refused (its drain shows the
  verbatim HTTP 403), and a client newer than the relay refuses to sign in when no
  challenge is issued ("update the relay"). The compose stack renders `protocol_version =
  2` into `yapstack-relay.toml`.
- Envelope version byte `0x01`, authenticated as the first AAD field (CRYPTO_SPEC C1).
- Server migrations are additive; the migrate one-shot re-runs on upgrade.

---

## 12. Invariants and where they are pinned

| # | Invariant | Test |
|---|---|---|
| I1 | The relay never stores plaintext | `sync_flow::plaintext_sentinel_never_appears_in_stored_server_bytes` |
| I2 | Cross-tenant reads are denied at the API | `sync_flow::cross_tenant_reads_are_denied_at_the_api`, `rls_nonowner_flow::*` |
| I3 | `changeset_seq` is dense, monotonic, commit-ordered, gap-free under concurrency | `sync_flow::push_assigns_dense_seqs_and_is_idempotent`, `concurrent_pushes_are_gap_free_and_monotonic` |
| I4 | Reused `client_seq` with different content is rejected, identical retry deduplicated | `sync_flow::push_rejects_seq_regression_but_allows_identical_retry` |
| I5 | Refresh-token reuse revokes the family, including concurrent reuse | `auth_flow::signup_login_refresh_and_reuse_detection`, `concurrent_reuse_of_same_fresh_token_revokes_family` |
| I6 | Roster counter strictly increases; concurrent uploads are compare-and-set safe | `auth_flow::roster_counter_anti_rollback_rejects_stale_or_equal`, `concurrent_roster_uploads_are_compare_and_set_safe` |
| I7 | Only an active caller may write the roster; a pending device enrols pending and is promoted by a roster | `auth_flow::pending_caller_put_roster_forbidden_active_accepted`, `unknown_device_enrolls_pending_then_roster_promotes` |
| I8 | Auth throttles cannot be rotated by a spoofed `X-Forwarded-For` | `client_ip_spoof::*` |
| I9 | Audio presign never reports a phantom done; refcount is mapping-count | `sync_flow::audio_presign_d8_refcount_and_dead_put_never_false_done`, `…already_exists_only_when_object_present` |
| I10 | GC deletes the object before the row and only past the grace period | `gc_flow::*` |
| I11 | Every §13 KAT vector reproduces, including the literal recovery split | `yapstack-crypto/tests/kat.rs`, `kat_recovery_regression.rs`, `audio_stream.rs` |
| I12 | Two devices converge through the relay; random programs converge | `runtime::two_devices_converge_through_relay`, `convergence_proptest` |
| I13 | Independent UUID libraries merge as a lossless union; shared ancestry is lossy by design | `merge_semantics::*` |
| I14 | Unknown-column changes quarantine and replay after migration | `remediations::r7_quarantine_replay_across_schema_gap` |
| I15 | Dedup and cascade never destroy a partially merged or not-yet-merged row | `data_integrity_prove::*` |
| I16 | A restored DB re-acquires its own history (own echoes merge) | `outbox` restore tests |
| I17 | Stale CRR triggers are detected and healed without touching sync state | `trigger_arity::*` |
| I18 | Audio round-trips byte-equal; wrong identity or truncation fails verification; cache is never written on failure | `audio_queue::*`, `audio_fetch::*` |
| I19 | A pending device cannot enable sync; sign-out keeps the anchor; an account switch resets watermarks and the outbox | `sync.rs` unit tests (`reconcile_account_binding_*`), `SyncSetupWizard.test.tsx` |
| I20 | One status derivation with the stated precedence; "Up to date" only when nothing is pending, behind, quarantined, failing or unreachable | `syncDisplay.test.ts`, `SyncStatusGlyph.test.tsx` |
| I21 | Failing drain cycles surface after two in a row and clear on success; a rejected refresh expires the session without a hot loop | `sync.rs` unit tests (`fail_surface_step`, `classify_refresh_status`) |
| I22 | The boot sweep never finalises or deletes a foreign recording | `db.rs` sweep matrix tests, `NoteDetailView.test.tsx` |
| I23 | A pending device cannot push or reserve an audio upload; pull stays open (§15.B) | `auth_flow::pending_caller_cannot_push_or_presign_active_can` |
| I24 | A recovery sign-in enrols the device and lets it re-bootstrap the roster exactly once; the grant never lands on another account's device (§15.A) | `auth_flow::recovery_enrols_device_and_lets_it_rebootstrap_roster_once`, `recovery_cannot_arm_a_grant_on_another_accounts_device` |
| I25 | A password change needs an active device plus the current `auth_key` or a fresh recovery waiver, keeps `salt_enc`, and revokes every other device's refresh families (§15.C) | `auth_flow::change_password_requires_reauth_or_fresh_recovery_and_keeps_salt` |
| I26 | Removing a device revokes its access, requires re-authentication, and re-enrols it pending with no grant; the removed device is told so and refuses to enable (§15.D) | `auth_flow::device_removal_revokes_access_and_requires_reapproval`, `device_removal_reauth_and_revoked_row_edges`, `SyncTab.test.tsx` ("a removed device is told so…") |
| I27 | A password holder cannot mint a token for another device's identity; a sign-in without a valid proof binds no device and cannot write (§15.K) | `auth_flow::password_holder_cannot_impersonate_another_device`, `yapstack-crypto sign::login_pop_message_field_order_and_domain` |
| I28 | The relay's plaintext roster counter must match the signature-bound one before the client signs anything (§5.4) | `sync.rs` unit test `relay_plaintext_counter_must_match_the_signed_one` |
| I-NT2 | The full secondary-index set of a freshly migrated non-CRR DB equals the set after cutover plus migrations; a `SyncIndex` migration never stales CRR triggers and refuses a non-PK UNIQUE (§4.3) | `r3_migration::crr_migration_preserves_secondary_indexes`, `db_service` tests `cutover_keeps_the_fresh_index_set`, `v18_restores_the_fresh_index_set_on_a_db_cut_over_without_indexes`, `sync_index_*` |

---

## 13. Where the earlier documents went

| Document | Status | Absorbed as |
|---|---|---|
| `~/Documents/Plans/YAPSTACK_SYNC_ARCHITECTURE.md` (outside the repo) | superseded for the repo's purposes | §1 posture, §3, §4, §11; every `architecture §N` reference in code and CRYPTO_SPEC maps per the table below |
| `docs/CRYPTO_SPEC.md` | **normative, unchanged** | cited throughout |
| `docs/self-hosting.md`, `deploy/README.md` | current operator docs | §2, §9.4 |
| `docs/plans/SYNC_REMEDIATION.md` | executed; historical | §6, §14 D19 |
| `docs/plans/SYNC_PAGE_REDESIGN.md` | executed, then superseded by the 2026-09-06 wizard decision | §10 |
| `docs/plans/AUDIO_ROUND_TRIP.md` | executed (S1–S3); S4 not built | §7 |
| `docs/plans/LIVE_SESSION_STATE.md` | executed | §8 |
| `SYNC_REVIEW_FINDINGS.md`, `SYNC_REMEDIATION_PLAN.md` (untracked, local) | every CONFIRMED finding has a landed fix commit; the PLAUSIBLE cutover race got only the no-CREATE guard | §14 D19, §15 |
| `docs/goals/sync-backend-v1/` (local board) | closed | owner decisions listed in §14 |
| `docs/plans/ENTITLEMENTS_SEAM.md` (local, not committed) | design source for the limits vocabulary, cited by the entitlements crate, the relay's admin/choke modules and migrations 0001/0002 | `crates/yapstack-entitlements` crate docs, this §9.4 (`[limits]`), §1.1 posture item 3 |

**Other citation vocabularies in code comments:** `T0NN` are goal-board task ids and
`RN` / `Item N` are remediation-round and review-finding numbers from the closed board and
the untracked review files — historical provenance, not documents a reader can open. A
bare `DN` outside this file refers to the decision list of the plan document the comment
sits next to (`LIVE_SESSION_STATE.md` D1–D8, `AUDIO_ROUND_TRIP.md` D6–D10), never to this
file's §14 register.

**`architecture §N` cross-reference** (code comments were re-pointed to these targets in
the merge cleanup; CRYPTO_SPEC keeps the original citations): §4.2/4.3 key scheme → CRYPTO_SPEC §2–§4; §4.4
bootstrap → this §5.3; §4.6 settings → this §4.5; §5 stack → this §3, §9.3; §6
migration → this §11; §6.3 quarantine → this §6.4; §6.4/6.5 floor and lifecycle →
not implemented (§14 D5); §7a/7b protocol → this §6.3–§6.4, §9.1; §8 shares → not
built; §9 audio → this §7 (format per CRYPTO_SPEC §1.5); §10 guardrails → this §9.1,
§9.4; §14 DTO placement → `yapstack-common`; §15 verification → this §12.

---

## 14. Divergence register (documents vs. implementation)

Each row states what a document said, what the code did on 2026-09-10, and the
resolution. Rows are resolved history: **Done (commit)** names the landing commit,
"Spec wins" means the document was wrong and this file is the reference, "Decision"
points to §15. Nothing in this table is still open.

| # | Source says | Code does | Resolution |
|---|---|---|---|
| D1 | The normative architecture is `YAPSTACK_SYNC_ARCHITECTURE.md`, cited as `architecture §N` by CRYPTO_SPEC and ~40 code comments | That file is not in the repository | **Spec wins.** §13 carries the mapping; the file is not to be committed |
| D2 | Decision report §4.2: two Argon2 passes with `salt_auth` and `salt_enc` | One pass + HKDF split (CRYPTO_SPEC §2.3) | CRYPTO_SPEC is authoritative; the report was a draft |
| D3 | Decision report §7a: SSE events carry `(changeset_seq, ciphertext)` | Wakeup events carry the seq only; pull is the truth | Spec wins (the blind hint is the safer design) |
| D4 | Decision report §9: `Content-MD5`, multipart > 100 MB, lazy re-hash, `GET /audio/{session_id}` | STREAM envelope, part-id addressing, pinned-length presigned PUT, AEAD tags as verification, no multipart | CRYPTO_SPEC §1.5 + this §7 are authoritative |
| D5 | Decision report §6.4–6.5: version handshake, per-tenant schema floor, device `active → stale → retired` | Only an advisory `min_client_version`; `devices.state` and `last_seen` are dead columns; no floor | Not implemented, documented (§11). Additive-only stays the safety lever; the columns stay (**§15.I**) |
| D6 | Decision report §8, CRYPTO_SPEC §8–§9: private-link shares and a JS share-viewer | `shares` table on both sides only; no routes, no UI, no JS stack | Not built; deferred. CRYPTO_SPEC §8/§9/§13.6 stay as the future contract |
| D7 | Decision report §4.6, Phase 3: per-field settings sync, AI keys vault-wrapped | `SETTINGS_SYNC_POLICY`, `projectSyncedSettings`, `stripAiSecrets`, `prepareAiConfigForSync` existed with no caller | **Done (78e0bf3):** `sync-policy.ts`, its test and the AI-wrap helpers deleted; the classification lives in §4.5 |
| D8 | Decision report Phase 3: default server URL is the hosted relay | `DEFAULT_SYNC_SERVER_URL = ""`; the hosted hostname does not resolve | Self-host only until a hosted relay exists (**§15.G**) |
| D9 | Decision report §6.1: a v2 device's unknown column is applied-known / quarantined-rest by v1 | Both true, but because the changeset AAD binds the **sender's** schema/engine version, any bump of `SYNC_SCHEMA_VERSION` quarantines every changeset from the newer device wholesale until the older updates and retries | Documented v1 posture; a version bump is a coordinated update. **§15.J** |
| D10 | `self-hosting.md`, SYNC_REMEDIATION §6b: snapshot seed/join "designed and partially built, not available" | Server `POST /snapshot/presign` + `GET /snapshot`, engine `snapshot.rs`/`reconcile.rs`, transport methods; no Tauri command | **Done (78e0bf3):** `reconcile.rs` and its tests deleted; `resite_as_fresh_peer` moved to `snapshot.rs` (the shared-ancestry test needs it); snapshot seam kept for compaction (§15.F) |
| D11 | CRYPTO_SPEC §7.5 step 3: a pending device "may pull/read" | The client refused `sync_enable` while pending; the relay gated only `PUT /devices/roster` — push, pull and audio were open to a pending token | **Done (78e0bf3, I23):** `devices::require_active_caller` gates push and audio presign (later also snapshot presign and password change); pull and audio GET stay open. `sync_enable` refuses a pending, removed or unlisted device with an honest message |
| D12 | CRYPTO_SPEC §4.3 password change (LOCKED flow) | No endpoint, no command, no UI | **Done (7820584, A9, I25):** `PUT /auth/password`, `sync_change_password`, Account row + post-recovery offer (§5.6) |
| D13 | CRYPTO_SPEC §4.4 vault-key rotation, §6.3 recovery-code revocation | Epoch is always 0; nothing rotates or re-wraps | Not built, deferred (§17); the anti-rollback plumbing (epoch in AADs, roster, session store) is in place |
| D14 | CRYPTO_SPEC §7.5: recovery re-establishes the device | `sync_recover` sent no device identity; the token had no `client_id`; the device was neither enrolled nor able to write the roster | **Done (23202d2, A7, I24):** recovery enrols the device and may re-bootstrap the roster once (§5.5) |
| D15 | CRYPTO_SPEC §10: vault key and device key live in the OS keychain | Identity in the keychain; the session (vault key + tokens) sealed in `sync-session.enc` under a keychain-held wrapping key in release; plaintext dev file in debug | **Done (c86d4eb):** CRYPTO_SPEC §10 amended (A8) to describe the sealed session store; recorded in §5.1 |
| D16 | CRYPTO_SPEC §7.3 roster entries: `client_id, ed25519_pub, label, added_at` | Same; `platform` is device-index metadata, **not** in the signed roster | Accurate; `platform` is advisory like `label` |
| D17 | `self-hosting.md`: "choose self-host, paste your relay URL"; "Settings → Sync → enable" on each device | The wizard has one server-address step (no hosted/self-host choice) and the second device must be approved before it can enable | **Done (c86d4eb):** doc updated |
| D18 | `CHANGELOG.md` Unreleased: sync "not yet included in official release builds" vs README "official releases include sync going forward" and `release.yml -f sync` | The release workflow on this branch builds with sync; no tag has shipped it yet | **Done (c86d4eb):** CHANGELOG wording updated |
| D19 | SYNC_REMEDIATION §3 tree-shake list; SYNC_REVIEW_FINDINGS 21 findings | All listed removals landed (`sync_info` → `sync_probe`, seed/join unregistered, device_list/wrap_secret/repair_oversized/legacy migration gone); all 21 findings have fix commits between `15ed390` and `517c699`; the audio lane's uninterruptible idle sleep remained a deferred nit | Plans marked historical; **Done (78e0bf3):** the audio lane's idle wait is 250 ms slices that re-check stop |
| D20 | SYNC_PAGE_REDESIGN §0.6: "single adaptive page, not a wizard" | Wizard from Settings, ratified 2026-09-06 | Plan marked superseded |
| D21 | AUDIO_ROUND_TRIP D10: blob GC deferred; S4 custom `audio_save_location` feeds the fetch cache | GC implemented (migration 0006, `gc.rs`); the fetch cache lives beside the database regardless of `audio_save_location`; `tenant_usage` was never decremented | GC done; S4 not built; **Done (78e0bf3):** the GC row-delete transaction decrements `tenant_usage.storage_bytes` (floored at 0) |
| D22 | Decision report §10: audit log of RLS denials; §15: nightly completeness check | Neither exists; completeness is read at drain spawn and its tips fold in per cycle; the dense cursor makes client-side gaps impossible | Not built; not needed for correctness. Drop from the contract |
| D23 | `lib/sync.ts` `SyncPhase` included `preparing`; `syncConfig.lastChangesetSeq` was persisted; `billingUrl` was a status field | Rust never emitted `preparing`; `lastChangesetSeq` had no writer; `build_status_dto` always set `billing_url: None` and the probe did not read it | **Done (78e0bf3):** removed (the relay's `GET /sync/info` still echoes an operator-configured `billing_url`; no client reads it) |
| D24 | `SyncError::CryptoSkip` docs: the drain **stops** at an undecryptable changeset | `pull_direction` quarantines and **advances** | **Done (78e0bf3):** removed |
| D25 | Relay compaction design (2026-08-28, local scratchpad) | Not implemented; the owner's relay holds ~12 000 changesets, so a fresh join replays all of them | Design written up as [`plans/RELAY_COMPACTION.md`](plans/RELAY_COMPACTION.md); **awaiting ratification** (§15.F) |
| D26 | ADR 0006 §2: the CRR rebuild keeps a table's secondary indexes | `transform_and_crrify` replayed triggers only, so cutover dropped every secondary index (the owner's synced DB kept just the two `lib/db.ts` re-creates); sessions-by-day and segments-by-session became full scans | **Done (FP2a):** indexes captured and replayed at cutover; migration v18 (`SyncIndex`) restores them on databases already cut over (§4.3, I-NT2) |
| D26 | `docs/ARCHITECTURE.md`, `INDEX.md`, `GLOSSARY.md` | No sync section, no sync entries, no sync terms | **Done (c86d4eb)**; re-aligned with the shipped slices in the merge docs pass |

---

## 15. Edge cases — owner decisions (ratified 2026-09-10)

Every item below was walked through with the owner on 2026-09-10. The chosen option is
recorded; the rationale from the review is kept so the reasoning survives.

**A. Lost-only-device recovery — DONE (slice 1, CRYPTO_SPEC A7).** A recovery-code
sign-in now enrols the device and lets it admit itself to the roster once, under strict
conditions, via a server-side one-shot `recovery_grant_at`. Independently reviewed
before landing (findings folded in: tenant-scoped grant, 15-min TTL, no third-party
promotion, old-relay handling, anchor fingerprint check). Vault-key rotation on
recovery stays deferred with §4.4. See §5.5 and §9.1.

**B. Server-side gating of pending devices — DONE (pass, 78e0bf3; I23).**
The relay rejects `POST /sync/push` and `POST /audio/presign` when the token's
`client_id` row is `pending`; `GET /sync/pull` and `GET /audio/part/{id}` stay open per
§7.5 step 3. The later slices put `POST /snapshot/presign` and `PUT /auth/password`
behind the same gate. A stolen password alone can no longer write to the relay.

**C. Password change — DONE (slice 2, CRYPTO_SPEC §4.3 + A9).** `PUT /auth/password`
with the new `auth_key` and password wrap; `salt_enc` retained (A9) so every device's C3
baseline survives; re-authentication with the current key, waived once within 15 minutes
of a recovery sign-in. Offered after recovery and from the Account row.

**D. Device removal — DONE (slice 4).** Approver re-authenticates and re-signs the
roster without the device; the relay marks it revoked, kills its refresh families and
refuses its writes; re-login or recovery re-enrols it pending with no grant. Independent
review folded in: re-auth, no grant for revoked rows, revoked self refused at enable,
snapshot write gated, honest copy about password holders and compromised relays. The
review also surfaced §15.K. See §5.7.

**E. Turning sync off — DONE (slice 3).** `sync_disable` stops the lanes and persists
`sync_enabled = false` while keeping the session; surfaced as "Turn off" on the Sync
page with a confirmation, and the hero offers "Turn on sync" afterwards. "Return to a
plain database" stays deferred.

**F. Relay history compaction — PROPOSED, awaiting the owner's ratification (not
started).** The design with a
recommendation per open question (sweep-first pruning below every active device's
acknowledged pull position, any caught-up device publishes size-triggered snapshots,
journaled bootstrap install that carries local-only tables) is
[`plans/RELAY_COMPACTION.md`](plans/RELAY_COMPACTION.md). It is held for the owner's
go because the bootstrap replaces a device's live database file.

**G. Hosted relay default — DECIDED: unchanged.** Flip `DEFAULT_SYNC_SERVER_URL` and
the wizard copy when a hosted relay launches.

**H. Settings sync — DONE (pass, 78e0bf3): deleted.** `lib/sync-policy.ts`, its test,
`stripAiSecrets` and `prepareAiConfigForSync` are gone. The per-field
classification is preserved as prose in §4.5; if settings sync is ever built it starts
from that list.

**I. Dead relay columns — DECIDED: leave them.** `devices.state`, `devices.last_seen`,
`shares`, `snapshots` stay; additive columns are harmless and a v1 relay with real data
does not earn a destructive migration.

**K. Proof of possession at login — DONE (slice 5b, 16653dd, CRYPTO_SPEC A10; I27).** `login/begin`
issues a single-use nonce; `login/finish` and `recover` carry a device-key signature over
it that the relay verifies against the stored key for a known `client_id`. A password
holder can no longer mint a token for another device's identity. Requests without a
valid proof bind no device and cannot write.

**L. Per-change `client_id` binding on push — OPEN, documented follow-on (surfaced by the
A10 review; not attempted in the merge passes).** The
write gate keys on the token's `client_id`, but each change in a push batch carries its
own `client_id`, which is the idempotency key and the stored author. An active device can
therefore push changes attributed to another in-tenant device and pre-occupy its
`(client_id, client_seq)` pairs (a spurious "counter regression" 409 on that device's
real pushes). Cross-tenant is impossible (RLS). Fix: reject any change whose `client_id`
differs from the caller's. Held for a follow-on because the relay's multi-device push
fixtures (`sync_flow.rs`) push several device streams through one token and need
re-authoring with per-device tokens first.

**J. Schema-version bumps — DECIDED: documented posture, auto-retry built when the
first bump is scheduled.** A `SYNC_SCHEMA_VERSION` bump quarantines updated devices'
changesets on not-yet-updated devices until they update; a once-per-update automatic
crypto-quarantine retry on the first drain after an app update will accompany the first
bump. The AAD binding is not relaxed.

## 16. Constants (single reference)

| Constant | Value | Where |
|---|---|---|
| Drain interval / write-kick debounce | 5 s / 400 ms | `sync.rs` |
| SSE reconnect backoff | 1 s → 30 s | `sync.rs` |
| Audio uploader idle interval / backfill stagger | 8 s / 30 s | `sync.rs` |
| Failure surfacing threshold | 2 consecutive cycles | `sync.rs` |
| Status poll (glyph) / approval poll | 5 s (2 s active) / 3 s | `SyncStatusGlyph.tsx`, `SyncSetupWizard.tsx` |
| Outbox chunk | 5 000 rows or 512 KiB plaintext, row-atomic | `outbox.rs` |
| Push wire budget / relay max batch | 1.5 MB / 1 000 changes, 5 MiB raw | `outbox.rs`, `yapstack-common` |
| Pull page | 500 (relay default 500, max 2 000) | `outbox.rs`, `pull.rs` |
| Access / refresh token TTL | 15 min / 30 days | `jwt.rs` |
| Presigned URL TTL | 15 min | `config.rs` |
| GC interval / grace | 24 h / 7 days | `config.rs` |
| Rate limits per minute | push 120 per (tenant, IP); login 10; signup 5 per IP | `config.rs` |
| Fetch concurrency / disk headroom / on-device re-probe | 2 / 256 MiB / 30 s | `audio.rs`, `lib/sync.ts` |
| Remote-live heartbeat threshold | 3 min | `NoteDetailView.tsx` |
| Argon2id client / server | m=65536 t=3 p=4 / m=19456 t=2 p=1 | CRYPTO_SPEC §12 |

---

## 17. Ratified plan (2026-09-10) — executed

Every item below has landed except the two marked as held. The merge-readiness passes of
2026-09-29 (`refactor(sync): simplify pass before merge`, `chore(sync): cleanup pass
before merge`, `docs(sync): align docs with as-built sync before merge`) cut against
§12/§15 only and built nothing new.

**Simplification pass (client + relay), fixed scope — executed 2026-09-10 (78e0bf3).** Cut
against §12 so no load-bearing test disappeared; the general sweep's findings that were
out of scope are listed in the pass commit message.

| Item | Change |
|---|---|
| D7 | delete `lib/sync-policy.ts` + test, `stripAiSecrets`, `prepareAiConfigForSync` |
| D10 | delete `crates/yapstack-sync/src/reconcile.rs` + `tests/reconcile.rs`; keep the snapshot seam |
| D23 | drop the `preparing` phase from the TS union, `syncConfig.lastChangesetSeq`, and the always-null billing-URL plumbing on the client |
| D24 | drop `SyncError::CryptoSkip` |
| D19 | audio lane idle wait becomes the 250 ms interruptible slice loop |
| D21 | GC row-delete transaction decrements `tenant_usage.storage_bytes` |
| D11 | relay rejects push and audio presign from a pending device (tests extend `auth_flow`) |
| — | general dead-code and duplication removal in `sync.rs`, `outbox.rs`, the server crate, and the sync UI |

**Feature slices after the pass, in order, each its own spec amendment + commit:**

1. Recovery re-bootstrap (§15.A, CRYPTO_SPEC A7) — done 2026-09-10, `23202d2`.
2. Password change (§15.C, CRYPTO_SPEC §4.3 + A9) — done 2026-09-10, `7820584`.
3. Turn off sync (§15.E) — done 2026-09-10, `e8617ae`.
4. Remove a device (§15.D) — done 2026-09-10, `afa3bf7`.
5. Proof of possession at login (§15.K, CRYPTO_SPEC A10) — done 2026-09-16, `16653dd`
   (relay migration 0013, protocol version 2).
6. Relay compaction (§15.F) — design proposed in `plans/RELAY_COMPACTION.md`;
   **held for the owner's ratification**.

**Held as a documented follow-on:** §15.L per-change `client_id` binding on push (needs
the relay's multi-device push fixtures re-authored with per-device tokens first).

**Deferred, documented only:** D5 version floor and device staleness, D9 auto-retry
until the first schema bump, D13 rotation and recovery-code revocation, D6 shares, D8
hosted default.
