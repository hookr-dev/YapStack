# YapStack Cryptography Specification (v1)

> **Status:** Normative. This is the single source of truth for all cryptographic
> primitives, parameters, byte layouts, and known-answer test (KAT) vectors used
> by YapStack sync. No server, sync-runtime, or share-viewer code may be written
> against constructions not specified here.
>
> **Scope:** One spec, two stacks. The Rust desktop client (`crates/yapstack-sync`,
> `apps/desktop/src-tauri`) implements this today and **MUST** pass the KAT vectors
> in §13 in CI. The JS/WASM share-viewer (planned; not yet in-tree) **MUST**
> implement byte-for-byte identical constructions and reproduce the same vectors
> before it ships — divergence means shares/audio/changesets that do not decrypt
> cross-platform, which is a release blocker.
>
> **Authority:** All choices below were fixed by prior adversarial review; they are
> normative and **MUST NOT** be relitigated in implementation without a new review
> round. Sections marked LOCKED additionally require a spec amendment (with its own
> review) before any change.
>
> **Keywords** MUST / MUST NOT / SHOULD / MAY are RFC 2119.

---

## 0. Threat model (one paragraph, so the rest is legible)

The server is an **honest-but-curious, potentially-compromised blind relay**. It
sees ciphertext, random IDs, KDF salts, wrapped keys, and version metadata — never
plaintext content, never a password, never a password-equivalent, never any
unwrapped key. Writers are **multiple offline devices** for the same account that
reconcile via a CRDT (cr-sqlite) and may encrypt concurrently without coordination
— so **nonce-collision resistance across independent devices is a first-class
requirement**, not an afterthought. An attacker may (a) dump the server DB, (b)
tamper with or reorder stored blobs, (c) roll back to an older stored blob, (d)
substitute one ciphertext for another. The design defends all four.

---

## 1. AEAD — the one content cipher

### 1.1 Primitive (LOCKED)

**All content encryption uses `XChaCha20-Poly1305` (IETF, 24-byte / 192-bit
nonce).** This covers changesets, audio blobs, shares, and every wrapped key.

**`AES-GCM` is FORBIDDEN for all multi-writer surfaces.** Rationale: AES-GCM has a
96-bit nonce; with multiple offline devices generating random nonces under a shared
key, birthday-bound nonce reuse is reachable, and GCM nonce reuse is catastrophic
(reveals the auth key → full forgery). XChaCha20-Poly1305's 192-bit random nonce
makes accidental collision cryptographically negligible (collision probability
< 2⁻⁹⁶ even at 2⁴⁸ messages), which is the property the offline-multi-writer model
requires.

### 1.2 Nonce generation (LOCKED)

- Every seal draws a fresh **24-byte nonce from a CSPRNG** (§11).
- Nonces are **random, never counter-based** (counters cannot be coordinated across
  offline devices). Random 192-bit nonces are collision-safe without coordination.
- A nonce is **never reused** for a different plaintext under the same key. Because
  every data key is single-use per item (§4), the practical reuse surface is a
  single (key, message) pair.

### 1.3 Sealed-envelope byte layout (LOCKED)

There are exactly **two** envelope shapes. Both begin with a version byte for
future agility (§11).

> **Version byte is authenticated (LOCKED).** The 1-byte envelope version is stored
> in the clear at the front of the blob (a decryptor must read it before it has a
> key), but it is **also bound as the FIRST AAD field** of every seal (§5.2). It is
> therefore covered by the AEAD tag — a tamper of the plaintext version byte makes
> `open()` fail. This closes the downgrade gap: "unknown version → quarantine" alone
> is insufficient once a *second* valid version exists (an attacker could flip
> `vN → v1` to force a deprecated construction). Because `v1` is bound into the tag,
> a flipped version simply fails authentication. See §5.2, §11.1, §5.3/§6.

**Standard envelope** — changesets, audio, synced settings:

```
sealed        = version(1) || nonce(24) || ciphertext(N) || tag(16)
                 ^0x01        ^XChaCha     ^ChaCha20 ct     ^Poly1305
                 └─ also bound as first AAD field (§5.2), authenticated by the tag
```

The AEAD library returns `ciphertext || tag` as one buffer; the layout is
`version || nonce || (ciphertext||tag)`. Minimum size = 1 + 24 + 0 + 16 = 41 bytes.

**Committing envelope** — shares and every wrapped-key blob (vault-key wraps,
recovery wraps, data-key wraps):

```
committing    = version(1) || commitment(32) || nonce(24) || ciphertext(N) || tag(16)
                 ^0x01        ^HKDF commit      ^XChaCha     ^ct              ^tag
                 └─ also bound as first AAD field (§5.2), authenticated by the tag
```

Minimum size = 1 + 32 + 24 + 0 + 16 = 73 bytes.

### 1.4 Key-committing construction (LOCKED) — required for shares & wrapped keys

Poly1305 (like all polynomial-MAC AEADs) is **not key-committing**: a ciphertext can
be crafted that decrypts successfully under two different keys. For shares (attacker
supplies both ciphertext and key-in-URL) and for wrapped keys (a malicious server can
swap wrapped blobs), this enables "invisible salamander"–class attacks. We therefore
wrap the AEAD in a **key-committing transform**.

**Construction: `UtC` (UNAE-then-Commit), the HKDF-derived commitment variant.**
Reference: Bellare & Hoang, *Efficient Schemes for Committing Authenticated
Encryption* (EUROCRYPT 2022), the `UtC` transform; and Albertini et al., *How to
Abuse and Fix Authenticated Encryption Without Key Commitment* (USENIX Security
2022). This is a **CMT-4-grade** transform, strictly stronger than
the Albertini "padding fix" (which is only compactly-committing); we adopt it
because shares are fully attacker-chosen.

> **A1 — precise commitment-level wording.** The 32-byte HKDF `commitment` is
> **key-committing (and nonce-committing**, since `salt = nonce`): it binds the
> envelope to exactly one `(K_root, nonce)` pair. **Full-context commitment** —
> also binding the ciphertext and the AAD (which includes the authenticated version
> byte, §5.2) — follows from *combining* the commitment with the Poly1305 AEAD tag:
> the commitment fixes the key and nonce, and the tag then fixes the
> `(ciphertext, aad)` under that fixed key. The composition (`UtC`) is what yields
> the CMT-4 property; neither piece alone does. The construction is sound; this note
> only makes the claim precise (the earlier phrasing loosely attributed full-context
> commitment to the HKDF commitment alone).

Given a root key `K_root` (a share key, recovery-wrapping key, or vault key) and a
freshly drawn 24-byte `nonce`:

```
okm(64)      = HKDF-SHA256(salt = nonce, ikm = K_root, info = "yapstack.commit.v1", L = 64)
commitment   = okm[0..32]        # prepended to the envelope
k_aead       = okm[32..64]       # the actual XChaCha20-Poly1305 key
# aad's FIRST field is the 1-byte version (§5.2, C1); the version byte is thus authenticated
ciphertext||tag = XChaCha20-Poly1305.seal(key = k_aead, nonce = nonce, pt, aad = LP(0x01, ...))
committing   = 0x01 || commitment || nonce || ciphertext||tag
```

`HKDF-SHA256` here is full RFC 5869 **Extract-then-Expand** (salt = nonce, so the
derivation is nonce-bound). Open recomputes `okm` from `K_root` and the envelope's
`nonce`, checks `commitment == okm[0..32]` in **constant time**, then AEAD-opens with
`k_aead` (whose `aad` re-includes the leading version byte, so a flipped version fails
the tag). A mismatch is a hard reject (quarantine, §11.3). Because `commitment` is a
32-byte PRF output over `(K_root, nonce)`, no second `(key, nonce)` can produce the
same envelope; combined with the AEAD tag over `(ciphertext, aad)` this binds the blob
to exactly one full context (A1 above).

> `info` string is the ASCII bytes `yapstack.commit.v1` = `796170737461636b2e636f6d6d69742e7631`.

### 1.5 Streaming AEAD envelope for audio — `yapstack.audio.stream.v1` (LOCKED)

> **Amendment (audio round-trip tranche, Judge-gated crypto review).** This section is
> **new, adversarially-reviewed crypto**. It defines the ONE audio blob format and
> **retires** the never-shipped whole-blob audio domain `yapstack.audio.v1` (§5.2). The
> retirement is safe *only* because **zero audio blobs exist in any deployment** (the
> `audio_blobs` / `audio_objects` tables are empty — the load-bearing invariant): there is
> no legacy ciphertext to migrate. This window closes the moment the first blob ships.

Audio blobs are large (the production library's largest single WAV is 599 MB). The
one-shot standard/committing envelopes (§1.3) take the whole plaintext as a single slice,
so constant-memory whole-blob sealing is **impossible** with them. Audio therefore uses a
**chunked STREAM** construction — **the maintained RustCrypto implementation
(`chacha20poly1305::aead::stream`, `StreamBE32` over XChaCha20-Poly1305); NEVER
hand-rolled.** It gives true **O(chunk) memory in both directions**, per-segment positional
binding via the counter, and last-block truncation detection.

**Blob layout (LOCKED):**

```
audio_blob = LP(wrapped_data_key)   # committing envelope §1.4/§4.2; wrap AAD binds identity (§4.2)
           || header                # version(1)=0x01 || chunk_size(u32be) || nonce_prefix(19)
           || seg_0 … seg_n         # STREAM segments; each = ct || tag(16); the last may be short

stream plaintext = original_audio_bytes   # the caller may prepend a 1-byte codec tag; opaque here
```

The **ciphertext hash that is content-addressed and uploaded is the whole `audio_blob`**
(from the leading `LP(wrapped_data_key)` through the last segment tag).

**Construction (LOCKED):**

- **Per-blob fresh random 32-byte data key** (§4), wrapped under `vault_key` with the
  committing envelope (§4.2) and stored as the leading `LP(wrapped_data_key)`.
- **Per-blob fresh random 19-byte `nonce_prefix`** (CSPRNG, §11.2). The per-segment nonce
  is `nonce_prefix(19) || counter(u32be) || last_flag(1)` — the 5-byte `StreamBE32`
  overhead — **assembled by the crate, never by us**. XChaCha's 24-byte nonce − 5 = 19.
- **Chunking:** `chunk_size` = **1 MiB plaintext per segment** (v1 constant, recorded in the
  header for agility). Each segment = `ct || tag(16)`; the last segment may be short. Memory
  bound: O(`chunk_size`) both directions, independent of blob size.
- **Last-segment discipline:** the final segment (even for an empty source — which yields
  exactly one last segment) is sealed with `encrypt_last` (`last_flag` set) and opened with
  `decrypt_last`. A **truncated** stream (dropped final segment) makes the previous
  `last_flag=0` segment be opened as `last_flag=1` → nonce mismatch → authentication
  failure. A **reordered / extended** stream shifts the counter → authentication failure.

**Header authentication (LOCKED):** the clear `header` (`version || chunk_size ||
nonce_prefix`) is passed as **AAD on every segment**, so header tamper (e.g. a `chunk_size`
flip) fails the first `open`. The version byte is the **first authenticated header field**
(C1 discipline, §5.2 / §11.1). A decryptor bounds `chunk_size` before allocating (a tampered
header cannot force an unbounded buffer).

**Identity binding at the wrap layer (LOCKED, D6):** identity is bound **only** on the
committing data-key wrap (§4.2), never inside the segments — so a `part_id` change never
requires re-encrypting content. A blob replayed under another tenant/session/part/epoch
fails at **unwrap**, before any segment is touched. See §4.2 and §5.2.

**Range access (documented, NOT v1):** the counter-addressable segments permit a future
ranged decrypt from segment boundaries. It is **explicitly not wired in v1** — v1 fetches
the whole blob before playing.

---

## 2. KDF — Argon2id, pinned client-side

### 2.1 Pinned client parameters (LOCKED, compiled into every client)

| Parameter | Value | Notes |
|---|---|---|
| Algorithm | **Argon2id** | hybrid; RFC 9106 |
| Version | **0x13** (19, v1.3) | never 0x10 |
| Memory (`m`) | **65536 KiB (64 MiB)** | |
| Iterations (`t`) | **3** | |
| Parallelism (`p`) | **4** | 4 lanes |
| Output length | **32 bytes** | feeds HKDF as the PRK |
| Salt | 16 bytes min, per-account, CSPRNG | server stores it; salt is not secret |

These parameters are a **compile-time constant** in both stacks. They are **NEVER
fetched from the server** — a server-supplied parameter is a downgrade vector (a
malicious relay could send `m=8, t=1` and make the auth verifier brute-forceable).
The server **MAY** store the per-account salt (it is not secret and is useless
without the password); it **MUST NOT** be able to influence `m/t/p/version/outlen`.

> **C3 — server salt-substitution / reuse (residual, documented; see §3.3).** The
> server *supplies* `salt_enc` to the client at login (§3.2). A **hostile** server can
> therefore substitute or **reuse a single fixed `salt_enc` across many accounts**,
> which enables **cross-user rainbow-table precomputation** against weak passwords —
> a precomputed `password → stretch` table then attacks every account sharing that
> salt at once (a path toward `password → master_key → E2E break` for weak passwords).
> Argon2id's cost only slows, it does not prevent, this once the salt is shared.
> **Mitigation (LOCKED for v1):** on **known devices**, the client **MUST cache its
> own `salt_enc` locally** (first-seen/TOFU) and **alert on any server-supplied
> mismatch** — a changed salt for an established account is treated as a
> tamper/downgrade signal, not silently accepted (§3.2, §3.3). This does not help a
> brand-new device (which has no cached salt), so cross-user salt reuse remains a
> **residual weakness of the v1 verifier flow, fully closed only by OPAQUE** (§3.3),
> where the server never chooses or sees a salt the client stretches under.

### 2.2 Hard client-side floor (LOCKED)

Both stacks embed a **floor check** that runs before any derivation and hard-fails
if the effective parameters are below `m=65536, t=3, p=4, v=0x13, outlen=32`. This
guards against a future config-loading bug or a tampered build re-introducing weak
params. Raising the floor in a later release is a normal versioned change (§11);
lowering it is forbidden.

### 2.3 Single Argon2 pass, then HKDF split (LOCKED)

We run **one** Argon2id pass (expensive) and split its output with **HKDF-SHA256
Expand** into an auth key and a master key, using **distinct `info` labels**. This is
the 1Password/Bitwarden split-KDF pattern and is strictly cheaper and no weaker than
two independent Argon2 passes with different salts, because the Argon2 output is a
uniformly-random 256-bit PRK and HKDF-Expand with domain-separated `info` yields
independent subkeys.

```
stretch(32) = Argon2id(password, salt_enc, m=65536, t=3, p=4, v=0x13, outlen=32)

auth_key(32)   = HKDF-SHA256-Expand(prk = stretch, info = "yapstack.auth.v1",   L = 32)
master_key(32) = HKDF-SHA256-Expand(prk = stretch, info = "yapstack.master.v1", L = 32)
```

- `salt_enc` is the single per-account Argon2 salt (there is **one** salt; separation
  is achieved by HKDF `info`, not by two salts — simpler and equivalent).
- HKDF here is **Expand-only** (RFC 5869 §2.3), because `stretch` is already a
  uniform 256-bit key (a valid PRK ≥ HashLen). No Extract step is needed or used.
- `auth_key` goes to the server at login (§3). `master_key` **never leaves the
  device** and only unwraps the vault key (§4).
- `info` strings (exact ASCII bytes):
  - `yapstack.auth.v1`   = `796170737461636b2e617574682e7631`
  - `yapstack.master.v1` = `796170737461636b2e6d61737465722e7631`

---

## 3. Auth verifier — the server never stores a password-equivalent

### 3.1 What the server stores (LOCKED)

The client sends `auth_key` (§2.3) — **not** the password, **not** `master_key` —
over TLS. `auth_key` is already a KDF output, but the server still **MUST NOT** store
it verbatim (a DB dump would then be a login-equivalent). The server stores a
**second hash**:

```
verifier = Argon2id(auth_key, server_salt, m=19456, t=2, p=1, v=0x13, outlen=32)
```

- `server_salt` ≥ 16 bytes, per-account, CSPRNG, stored alongside.
- Server params are **lighter than client params** (`m=19456 KiB, t=2, p=1`) on
  purpose: `auth_key` is already a 256-bit high-entropy value (not a low-entropy
  password), so the server-side hash only needs to slow a DB-dump attacker, not
  resist a dictionary attack. It stays cheap enough for login throughput.

**Chosen second-hash primitive: Argon2id** (not bcrypt, not scrypt). Justification:
(1) bcrypt truncates inputs at 72 bytes and mangles embedded NUL bytes — hostile to a
32-byte binary `auth_key`; (2) using one primitive (Argon2id) across the whole
codebase shrinks the audit surface and reuses the same vetted library; (3) scrypt is
acceptable but offers no advantage here. Memory-hardness is belt-and-suspenders given
the high-entropy input.

### 3.2 Login / signup wire flow (LOCKED)

**Signup** (`POST /auth/signup`): client generates `salt_enc` (16 B), derives
`stretch → auth_key, master_key`, generates the vault key and its wraps (§4),
generates a `server_salt`-less payload (server picks `server_salt`), and sends:

```
{ email, auth_key(b64), salt_enc(b64),
  wrapped_vault_key_password(b64),      # committing envelope, §4
  wrapped_vault_key_recovery(b64),      # committing envelope, §6
  device_list(signed, §7) }
```

Server computes `verifier = Argon2id(auth_key, server_salt=fresh)`, stores
`{email, verifier, server_salt, salt_enc, wrapped_vault_key_password,
wrapped_vault_key_recovery, device_list}`. Server **discards `auth_key` immediately**
after computing `verifier` — it is never persisted.

> **C2 (first-device self-enrollment).** At signup the signing device **authors the
> initial signed roster itself**: it derives `devlist_sign_seed` from the fresh
> `vault_key` (§7.2) and signs a `device_list` containing exactly this one device,
> with `counter = 0` and **`vault_key_epoch = 0`** (§7.3). This is the account's
> first-bootstrap **TOFU anchor** (§7.4): no prior roster exists to compare against,
> so `epoch 0` is trusted on faith and all later rosters must strictly advance
> `counter`/`epoch` from it (§7.4, §7.5).
> **C3 (client caches its own `salt_enc`).** The signup device records the `salt_enc`
> it generated in local encrypted state as the first-seen baseline for §3.2 login
> mismatch alerting.

**Login** (`POST /auth/login`): two-round to avoid shipping the salt blindly.
1. Client → `{email}`; Server → `{salt_enc}` (the Argon2 salt; if email unknown,
   return a **decoy salt** derived deterministically as `HKDF(server_pepper, email)`
   to prevent account-existence oracles). **C3 mismatch check:** for a **known device**
   (one that has previously bootstrapped this account and cached its own `salt_enc`),
   the client **MUST compare** the served `salt_enc` against its locally cached value
   and **alert / refuse to proceed on mismatch** — a changed salt for an established
   account signals a hostile relay substituting a shared/reused salt for cross-user
   precomputation (§2.1). A first-time device has no cached salt and accepts the
   served one TOFU (the residual §3.3 closes only under OPAQUE).
2. Client derives `auth_key`, sends `{email, auth_key}`; server recomputes
   `Argon2id(auth_key, server_salt)` and compares to stored `verifier` in constant
   time. On success, server returns access JWT (15 min) + refresh (30 d, with
   rotation + reuse detection per architecture §5/§10), plus `salt_enc`,
   `wrapped_vault_key_password`, and the signed `device_list` for bootstrap (§7).

### 3.3 OPAQUE — documented upgrade path (NOT v1)

The above is a "salted-second-hash of a high-entropy verifier over TLS" — it exposes
`auth_key` to a fully-malicious server at login (a hostile relay could capture it).
This is acceptable for v1 because `auth_key` is domain-separated from `master_key`
(capturing it does **not** yield the encryption key). The documented upgrade is an
**aPAKE (OPAQUE, RFC 9497 primitives / CFRG draft)**: the server never receives even
`auth_key`; login is a zero-knowledge proof of the password against a stored
envelope. Wire implications for the future: replace §3.2 round 2 with the OPAQUE
`KE1/KE2/KE3` triple, store the OPAQUE `registration_record` instead of
`{verifier, server_salt}`, and derive `master_key` from the OPAQUE `export_key`
instead of a second HKDF label. Envelope (§4) is unchanged. Flagged for the T006
reviewer as the known residual weakness of v1.

**Residual weaknesses of the v1 verifier flow (both closed by OPAQUE):**
1. **`auth_key` capture at login** — a fully-malicious relay sees `auth_key` on the
   wire. Bounded: `auth_key` is domain-separated from `master_key`, so capture does
   **not** yield the encryption key.
2. **Server salt-substitution / cross-user salt reuse (C3, §2.1)** — because the
   server supplies `salt_enc`, a hostile relay can serve one fixed salt to many
   accounts to enable cross-user precomputation against weak passwords. Mitigated on
   **known devices** by the client-cached-`salt_enc` mismatch alert (§3.2); **not**
   mitigated for a brand-new device. OPAQUE removes the server's ability to choose or
   observe the salt the client stretches under, closing this fully.

---

## 4. Envelope scheme — master → vault → data keys

### 4.1 Key hierarchy (LOCKED)

```
password ─Argon2id+HKDF─▶ master_key ──unwraps──▶ vault_key (random 256-bit, per account)
                                                      │ wraps
                          ┌───────────────────────────┼───────────────────────────┐
                          ▼                            ▼                            ▼
                    per-changeset data key      per-audio-blob data key      per-share key (§8)
```

- `vault_key` = **32 random bytes (CSPRNG), one per account.** It is the only key
  that encrypts (via derived data keys) actual content.
- `master_key` (from the password) wraps `vault_key`. The **recovery code** (§6)
  independently wraps the same `vault_key`. Both wraps use the **committing
  envelope** (§1.4).
- Each changeset batch and each audio blob gets a **fresh random data key** (32 B),
  wrapped under `vault_key` (committing envelope) and stored **next to its
  ciphertext** in the encrypted payload. Rationale for per-item data keys: bounds
  nonce reuse to a single (key, message) pair and makes future selective re-wrap /
  rotation (§4.4) possible without re-encrypting content.

### 4.2 Wrapped-key format (LOCKED, versioned)

Every wrapped key is a **committing envelope** (§1.4) whose plaintext is the 32-byte
wrapped key and whose AAD is a domain string identifying the wrap type:

```
wrapped_vault_key_password = committing_seal(K_root = master_key, pt = vault_key,
                                             aad = LP("yapstack.wrap.vault.pw.v1"))
wrapped_vault_key_recovery = committing_seal(K_root = recovery_key, pt = vault_key,
                                             aad = LP("yapstack.wrap.vault.rec.v1"))
wrapped_data_key           = committing_seal(K_root = vault_key,   pt = data_key,
                                             aad = LP("yapstack.wrap.data.v1", epoch_u32))
wrapped_audio_data_key     = committing_seal(K_root = vault_key,   pt = audio_data_key,
                                             aad = LP("yapstack.wrap.audio.stream.v1",
                                                      tenant_id, session_id, part_id, epoch_u32))
```

`epoch_u32` = the vault-key rotation epoch (§4.4), big-endian, so a data key wrapped
under vault-key generation N cannot be silently reinterpreted under generation N+1.
`LP(...)` is the canonical length-prefixed encoding of §5.

> **Audio data-key wrap (LOCKED, amendment §1.5 / D6).** The per-audio-blob data key
> (§1.5) is wrapped with a **distinct** wrap domain `yapstack.wrap.audio.stream.v1` that
> **also binds the blob's identity tuple** `(tenant_id, session_id, part_id)` in addition
> to `epoch_u32`. `part_id` (not `part_index`) is the `session_audio_parts` row UUID (D6):
> the CRR rebuild strips `UNIQUE(session_id, part_index)`, so `part_index` can collide
> cross-device, but a CSPRNG UUIDv4 row id cannot. A blob addressed to a different
> tenant/session/part, or wrapped under a stale vault epoch, therefore **fails at unwrap
> before any segment is decrypted**. As with every wrapped key, `version` is the first AAD
> field (C1).

### 4.3 Password change (LOCKED)

Re-derive `master_key'` from the new password + a **fresh `salt_enc'`**, re-wrap
**only** `vault_key` under `master_key'`, upload `{new auth_key, salt_enc',
wrapped_vault_key_password'}`. **No content is re-encrypted.** The recovery wrap is
untouched. Server updates `verifier`, `server_salt`, `salt_enc`, and the password
wrap atomically.

### 4.4 Vault-key rotation (LOCKED — flow specified even though the UI ships later)

Rotation is required for **post-compromise remediation** (a device/vault key was
exposed) and for **future Teams member removal** (evict a departed member's access).
Flow:

1. Generate `vault_key' = 32 random bytes`; increment `vault_key_epoch` (a monotonic
   u32 stored in the signed device list, §7).
2. Re-wrap `vault_key'` under the current `master_key` and under the recovery key
   (both committing envelopes) and upload the new wraps.
3. Re-derive the Ed25519 device-list signing key from `vault_key'` (§7), bump the
   device-list `counter` and `vault_key_epoch`, re-sign the roster, upload.
4. **Re-wrap all per-item data keys** from `vault_key` to `vault_key'`. This may be
   lazy (a background pass) — new writes immediately use `vault_key'`; reads try
   `vault_key'` then fall back to a retained `vault_key` generation from an encrypted
   local keyring until the pass completes.

> **Honest caveat (for the reviewer and the ADR):** rotation re-establishes secrecy
> for **future** writes immediately, but any party who captured `vault_key` and the
> **old ciphertext** can still read that old content until the re-wrap **and a
> re-encrypt of the underlying content** completes. Full post-compromise remediation
> of *existing* content therefore requires re-encrypting content under fresh data
> keys, not merely re-wrapping. v1 specifies the key-rotation mechanism; the
> content-re-encrypt sweep is a documented follow-on and the rotation UI ships later.

---

## 5. AAD binding — canonical, length-prefixed, unambiguous

### 5.1 Canonical encoding `LP` (LOCKED)

AAD is the concatenation of **length-prefixed fields**. Each field is
`uint32_be(len) || bytes`. This is unambiguous regardless of field contents (no
delimiter can be forged by field data). Numeric fields are fixed-width big-endian
byte strings, then length-prefixed like any other field.

```
LP(f0, f1, ..., fn) = ( u32be(len(f0)) || f0 ) || ( u32be(len(f1)) || f1 ) || ...
```

Field wire types:

| Field | Wire type |
|---|---|
| `version` | **1 raw byte** (`0x01`) — the envelope version byte, C1 |
| `domain` | ASCII bytes (e.g. `yapstack.changeset.v1`) |
| `tenant_id`, `client_id`, `session_id`, `share_id` | UUID → **16 raw bytes** |
| `client_seq` | **u64 big-endian** (8 bytes) |
| `schema_version` | **u32 big-endian** (4 bytes) |
| `engine_version` | **u32 big-endian** (4 bytes), encoded `major*1_000_000 + minor*1_000 + patch` (cr-sqlite `0.16.3` → `16003`) |

### 5.2 Bound fields per surface (LOCKED)

**C1 — the 1-byte `version` (`0x01`) is the FIRST AAD field on EVERY surface** (both
standard and committing envelopes). It is the same byte written in the clear at the
front of the blob (§1.3), now also authenticated by the tag so it cannot be
downgraded. Wrapped-key AADs (§4.2) likewise gain `version` as their first field.

- **Changesets:**
  `AAD = LP(version, "yapstack.changeset.v1", tenant_id, client_id, client_seq, schema_version, engine_version)`
- **DB snapshots (R2 bootstrap — registry gap-fill, DOCUMENTS SHIPPED BEHAVIOR, not a change):**
  `AAD = LP(version, "yapstack.snapshot.v1", tenant_id, generation_u64)`, with the data-key
  wrap under its own domain `LP(version, "yapstack.wrap.snapshot.v1", epoch_u32)` (§4.2).
  `generation_u64` is big-endian. A snapshot is the seed device's compact CRR database
  file, sealed with the same two-envelope construction as a changeset (fresh random data
  key wrapped under the vault key, payload sealed under that data key) but bound to
  `generation` instead of `(client_id, client_seq)`; the distinct wrap domain keeps a
  snapshot data key from ever being opened as a changeset one. These are the AADs
  `crates/yapstack-sync/src/crypto.rs` has built since R2 shipped — recorded here because
  they were missing from this table, not newly specified.
- **Audio blobs (RETIRED — never shipped, amendment §1.5):**
  `AAD = LP(version, "yapstack.audio.v1", tenant_id, session_id)`. This whole-blob domain
  is **retired**: zero blobs were ever sealed under it (the empty-tables invariant). Audio
  now uses the streaming construction (§1.5): the segment AAD is the **clear header**, and
  identity is bound on the **data-key wrap** under `yapstack.wrap.audio.stream.v1` (§4.2),
  which carries `(tenant_id, session_id, part_id, epoch_u32)`. There is **no dual format**.
- **Shares:**
  `AAD = LP(version, "yapstack.share.v1", share_id)`
- **Wrapped keys (§4.2):**
  `AAD = LP(version, "yapstack.wrap.<...>.v1"[, epoch_u32])`

### 5.3 Seq-binding decision (LOCKED, documented)

The AAD binds **`client_seq`** (client-chosen, per-client-monotonic), **NOT** the
server-assigned `changeset_seq`. Reason: per architecture §7a the server assigns
`changeset_seq` **at commit**, so the client cannot know it at encrypt time; binding
it would force a decrypt-re-encrypt round-trip through the server, defeating the
blind relay. `(client_id, client_seq)` is globally unique (it is also the push
idempotency key), so binding it prevents an attacker from replaying tenant A's
changeset as tenant B's or reordering a client's own stream. The server
`changeset_seq` is a **dense commit-ordered cursor**; its integrity is protected by
TLS in transit and by the **completeness/anti-entropy endpoint** (architecture §7b),
not by the AEAD. This split is deliberate and is the correct trust boundary for a
blind relay.

### 5.4 Authenticated version is authoritative for §6 (LOCKED, C4)

The `schema_version` and `engine_version` bound into changeset AAD (§5.2) are
**authenticated**: they are exactly the values that produced a **successful
`open()`**. All **schema-desync quarantine and version-handshake decisions of
architecture §6 MUST key off these AAD-bound values, NOT** off the plaintext
`schema_version` / `engine_version` **metadata columns the server exposes alongside
the blob**. Those server-side columns are **untrusted hints** only (usable for
pre-decrypt routing/back-pressure, never for a trust decision): a blind-but-hostile
relay can set them to anything. The rule:

- Decrypt first; the version that authenticated is the truth.
- If the server's plaintext hint **disagrees** with the AAD-authenticated version of
  a blob that opened, treat the item as tampered → **crypto-quarantine** (§11.3) and
  surface a diagnostic; do **not** act on the hint.
- A blob that fails to open under the local engine's expected version follows the
  normal §6.3 schema-desync path; the authenticated version (once a candidate opens)
  tells you *which* gap.

This makes the AEAD tag — not the server's columns — the authority for every §6
version gate, consistent with C1 (the version byte itself is authenticated).

---

## 6. Recovery code

### 6.1 Generation & format (LOCKED)

- **20 bytes (160 bits) from a CSPRNG**, ≥ 128-bit floor. **System-generated,
  NEVER user-chosen** (user-chosen = low entropy = server-side brute of the recovery
  wrap).
- Display: **RFC 4648 base32 (uppercase, no padding)** → 32 chars, shown in **8
  groups of 4** joined by `-`:
  `AAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGG-HHHH`. Input parsing is case-insensitive and
  strips hyphens/whitespace before base32-decode.
- Onboarding **MUST force** the user to record it (per architecture §4.3 / §11.1);
  the UI must state plainly that **lost password + lost recovery code =
  unrecoverable data** (there is no server-side reset in true E2E).

### 6.2 Deriving the recovery-wrapping key and recovery auth key (LOCKED)

The recovery code is already high-entropy (160 bits), so **no Argon2 pass is needed
or used** — Argon2 defends low-entropy secrets; a 160-bit random value is not
brute-forceable. The recovery code plays **two roles** — it *wraps* the vault key
(offline) **and** it *authenticates* to `POST /auth/recover` (online, so the server
will serve the recovery-wrapped blob, §3.1). These MUST use **domain-separated**
keys so the online auth token can never expose the offline wrap key. This exactly
mirrors the password path's `auth_key`/`master_key` split (§2.3): the password is
stretched then split; the recovery code is (no stretch needed) split. Derive **one
64-byte HKDF-Expand** over the locked `yapstack.recovery.v1` PRK and split it into two
32-byte blocks:

```
okm(64)              = HKDF-SHA256-Expand(prk = recovery_bytes_20, info = "yapstack.recovery.v1", L = 64)
recovery_key(32)     = okm[0..32]     # the vault-WRAP key (offline)
recovery_auth_key(32)= okm[32..64]    # the recovery AUTH key (online, → /auth/recover)
```

- **`recovery_key` = `okm[0..32]` is BYTE-IDENTICAL to the earlier 32-byte
  `HKDF-Expand(..., L=32)`** (HKDF-Expand block 1 = `T(1)` is the same first 32 bytes
  regardless of the requested output length, RFC 5869 §2.3). **The on-disk
  `wrapped_vault_key_recovery` is therefore UNCHANGED** — this ratification adds a
  second block, it does not alter the wrap. `wrapped_vault_key_recovery =
  committing_seal(recovery_key, vault_key, ...)` per §4.2 exactly as before.
- **`recovery_auth_key` = `okm[32..64]`** is the recovery-side analog of the password
  `auth_key`: the client sends it to `POST /auth/recover`, and the server
  **second-hashes it** into a stored `recovery_verifier` (Argon2id with the server
  params + a per-account `recovery_salt`, §3.1) and discards it — never storing a
  password-equivalent, never able to recover the code. It is a **distinct** value from
  `recovery_key`, so disclosing the online auth token (or its server-side verifier)
  reveals nothing about the offline wrap key.

HKDF here is Expand-only over the 160-bit input (acceptable: input entropy ≥ 128 bits,
the wrap is committing, and domain separation between the two blocks is provided by
HKDF's per-block counter). See §13.7 for the KAT.

### 6.3 Revocation caveat (LOCKED, documented)

"Revoking" a recovery code = generating a new one and overwriting
`wrapped_vault_key_recovery` on the server. **Caveat:** if the server (or an attacker
with server access) retained the *old* wrapped blob, the old recovery code can still
unwrap the vault key from that retained copy — client-side revocation is
**unenforceable against a malicious server**. The only true remediation is
**vault-key rotation** (§4.4), which makes every old *wrap* useless for future
content. **C5 caveat (do not overpromise):** rotation stops the old recovery code
from unwrapping the **new** `vault_key`, but by itself it does **NOT** remediate
**already-captured OLD content** — anyone who kept the old recovery wrap plus the old
ciphertext can still decrypt that old content until a **content re-encrypt sweep**
under fresh data keys completes (§4.4's honest caveat). The UI must therefore promise
no more than: "revoke = new code; the old code stops working via our client. To be
certain **against future** compromise, rotate the vault key (§4.4) — and note that
fully remediating **already-exposed** content additionally requires the re-encrypt
sweep described in §4.4, not rotation alone."

---

## 7. Device authorization & signed device list

### 7.1 Client identity (LOCKED)

- Each install generates a **fresh random `client_id` = UUID v4** (CSPRNG). Never
  derived from hardware, never reused across reinstalls.
- Each device generates an **Ed25519 device keypair** at first run; the private key
  is stored in the OS keychain (§10).

### 7.2 Device-list signing key (LOCKED)

The signed device list (roster) is signed with an **Ed25519 key derived from the
vault key** — so any device holding the vault key can author the roster, and rotating
the vault key rotates the roster authority (Teams eviction):

```
devlist_sign_seed(32) = HKDF-SHA256-Expand(prk = vault_key, info = "yapstack.devicelist.sign.v1", L = 32)
# Ed25519 keypair from this 32-byte seed (RFC 8032)
```

### 7.3 Signed device list (LOCKED)

The roster is a canonical structure signed by the vault-derived Ed25519 key:

```
device_list = {
  version: 1,
  tenant_id: UUID,
  vault_key_epoch: u32,          # bumped on rotation (§4.4)
  counter: u64,                  # STRICTLY MONOTONIC, bumped on every roster change
  devices: [ { client_id: UUID, ed25519_pub: 32B, label: str, added_at: rfc3339 }, ... ]
}
signature = Ed25519.sign(devlist_sign_seed, canonical_bytes(device_list))
```

`canonical_bytes` = `LP(§5)` over the fields in the order above (numeric fields
u32/u64 big-endian, arrays length-prefixed by element count then each element
length-prefixed). The server stores `{device_list, signature}` opaquely and serves
it on bootstrap; it cannot forge it (no vault key).

### 7.4 Anti-rollback (LOCKED)

- The **server records the highest `counter` it has ever accepted** and **rejects any
  uploaded roster with `counter ≤` stored** (prevents an attacker re-uploading an old
  roster to re-admit an evicted device).
- **Clients** reject a served roster whose `counter` is **lower** than the highest
  they have previously verified for this account (cached locally), and reject a
  `wrapped_vault_key_*` whose embedded `epoch` (§4.2) is **lower** than the roster's
  `vault_key_epoch` — this is the **wrapped-vault-key freshness / anti-rollback**
  guard: a malicious server cannot serve a pre-rotation wrapped vault key.

> **C2 — first-bootstrap has no baseline: TOFU (LOCKED, documented).** The client
> rollback rejects above compare against a **locally-cached** highest
> `counter`/`vault_key_epoch`. A **fresh device performing its very first bootstrap
> (§7.5) has no such baseline**, so it cannot detect rollback on its own: a malicious
> server can serve a **self-consistent but STALE** `(roster, wrapped_vault_key)` pair
> — both from the same pre-rotation epoch, so the signature verifies and the embedded
> `epoch` is not "lower than" anything the new device knows — and thereby roll the new
> device back to a **pre-rotation `vault_key` epoch**. First bootstrap is therefore
> **Trust-On-First-Use (TOFU) for `vault_key_epoch`**; the accepted epoch is only an
> assumption, not a verified fact. This gap is **closed / self-healed** by the
> approval ceremony (§7.5): (a) the approving existing device holds the *true* current
> epoch and re-signs the roster with a **bumped `counter`/`epoch`**, re-anchoring the
> new device to the live epoch; and (b) an **out-of-band roster-fingerprint + epoch
> check** during approval lets a human catch a server that served a stale roster. A
> new device **MUST NOT** be treated as fully trusted (able to author rosters / do
> post-rotation-sensitive actions) until it has been through approval and observed a
> `counter`/`epoch` advance signed by an already-trusted device.

### 7.5 New-device bootstrap ceremony (LOCKED)

**First-device special case (self-enrollment).** The account's **first** device does
not run this ceremony — there is no existing device to approve it. At signup it
**authors the initial signed roster itself** (§3.2, C2): one device, `counter = 0`,
`vault_key_epoch = 0`. That epoch-0 roster is the account's TOFU anchor (§7.4); every
subsequent device joins via the ceremony below.

> **Bootstrap `tenant_id = nil` and re-anchor-at-first-approve (LOCKED, documented,
> accepted).** At signup the client does **not yet know its `workspace_id`** — the
> server assigns it and returns it in the signup response. The first device therefore
> signs the epoch-0 roster with **`tenant_id = nil` (all-zero UUID)** as a placeholder.
> This is **safe** because roster verification (§7.5 step 2, §7.4) checks the
> **signature** (authored by a vault-key holder) and the **`counter`/`epoch`**
> monotonicity — it does **NOT** predicate trust on `tenant_id` equality (the workspace
> binding is already enforced server-side: the RLS `tenant_id` comes only from the
> validated JWT, §5, never from the opaque roster the relay cannot read). The **true
> `tenant_id` is re-anchored at the first approval**: when an active device runs the
> ceremony below it re-signs the roster (bumped `counter`) and MAY populate the real
> `tenant_id`; because clients ignore the roster `tenant_id` for trust, no separate
> post-signup roster re-sign is required for correctness. Accepted as-implemented (a
> two-device account never depends on the bootstrap roster's `tenant_id`).

1. New device: fresh `client_id` (UUID v4) + Ed25519 keypair. Password login (§3.2)
   returns `salt_enc`, `wrapped_vault_key_password`, and the signed roster. The
   wrapped vault key and salts are inert without the password, so the server hands
   them out freely (architecture §4.4). **The new device treats the served
   `counter`/`vault_key_epoch` as TOFU (§7.4): it has no baseline, so it cannot yet
   know the server didn't hand it a stale pre-rotation pair.**
2. New device derives `master_key`, unwraps `vault_key`, verifies the roster
   signature and `counter`/`epoch` (§7.4). A valid signature only proves the roster
   was authored by *a* vault-key holder at *some* epoch — **not** that it is the
   *current* epoch.
3. **Device approval (added defense against a stolen password; also the TOFU
   self-heal):** the new device is **pending** until an **existing device** adds its
   `client_id` + Ed25519 pub to the roster, **bumps `counter` (and re-asserts the true
   current `vault_key_epoch`)**, re-signs, and uploads. The approving device holds the
   *live* epoch, so this **re-anchors the new device to the current epoch and
   self-heals** any stale roster the server fed it in step 1. Until approved, the new
   device may pull/read but is flagged pending; other devices surface "New device
   requesting access: <fingerprint>". This makes a password thief's silent new device
   **visible and excludable** rather than invisible.
   > **Server-side write gate (defense-in-depth).** The relay additionally rejects a
   > `PUT /devices/roster` unless the **caller's own device is `active`** (identified by
   > the `client_id` bound into its access token, never request-supplied). Membership is
   > still the client-verified signed roster (the relay is blind), but this stops a
   > **pending / unapproved caller** (or a stolen pending-device token) from overwriting
   > the roster (availability/DoS) or self-promoting via a crafted roster — an honest
   > device would reject such a roster on full verification anyway, and now the relay
   > refuses to store it at all. Signup is the bootstrap exception: it writes the FIRST
   > active roster directly (no active device exists yet).
4. **Out-of-band roster check at approval (LOCKED, C2).** During approval the
   **existing (already-trusted) device displays the current roster fingerprint AND
   `vault_key_epoch`**; the **new device displays what it received in step 1** and the
   human **compares both out-of-band**. A mismatch (different fingerprint or a lower
   epoch on the new device) means the server served the new device a **stale/rolled-back
   roster** — abort and re-fetch. This is the human check that closes the first-bootstrap
   TOFU rollback (§7.4): the approving device's live epoch is authoritative.
5. **Device fingerprint (display):** `SHA-256(ed25519_pub)` → first 10 bytes →
   RFC 4648 base32 uppercase → **4 groups of 4** (`AAAA-BBBB-CCCC-DDDD`), shown on
   both devices for out-of-band comparison during approval. The **roster fingerprint**
   (for the epoch check) is `SHA-256(canonical_bytes(device_list))` → first 10 bytes →
   same base32 4×4 grouping, displayed next to the `vault_key_epoch`.

---

## 8. Share keys

### 8.1 Scheme (LOCKED)

- Per share: **`K_share` = 32 random bytes (CSPRNG)**, independent of the vault key
  (a leaked share key exposes exactly one note, nothing else).
- Encrypt the note's rendered HTML/markdown with the **committing envelope** (§1.4,
  §1.3) under `K_share`, AAD = `LP("yapstack.share.v1", share_id)`. `POST /shares`
  uploads `{share_id, committing_ciphertext}`; the server stores ciphertext only.
- **Key-in-URL-fragment:** `https://yapstack.app/s/{share_id}#k={base64url(K_share)}`.
  The fragment (`#...`) is **never sent to the server** by browsers; the viewer reads
  it from `location.hash`, fetches ciphertext by `share_id`, decrypts locally.
- Committing AEAD is **mandatory** here (not optional): the attacker controls both
  the ciphertext (uploads it) and the key (in the URL), which is exactly the setting
  where non-committing AEAD breaks.

### 8.2 Share-viewer constraints (LOCKED)

- HTTP header **`Referrer-Policy: no-referrer`** on the viewer page (so the `#k=`
  fragment never leaks via `Referer` even on outbound links in rendered content).
- **No third-party JavaScript, no analytics, no external subresources** — the viewer
  is fully self-contained static assets; the fragment key must never be exfiltrable.
  Enforce with a strict **CSP** (`default-src 'self'`, no `connect-src` to third
  parties beyond the ciphertext origin).
- Rendered share content is sandboxed (no script execution from note bodies) and
  outbound links get `rel="noreferrer noopener"`.

---

## 9. Crypto parity across two stacks

### 9.1 Library choice (LOCKED, justified)

WebCrypto (`SubtleCrypto`) provides **neither XChaCha20-Poly1305 nor Argon2id**, so
the share-viewer cannot use it for these primitives.

- **JS / WASM (share-viewer):** **`@noble/ciphers`** (XChaCha20-Poly1305) +
  **`@noble/hashes`** (HKDF-SHA256, SHA-256, Argon2id) + **`@noble/curves`**
  (Ed25519). Chosen over `libsodium.js` for **supply-chain posture**: the noble
  libraries are **zero-dependency**, small, individually auditable pure-TS, audited
  (Cure53), and tree-shakeable — a far smaller and more legible trusted-code surface
  than a large emscripten libsodium blob. `libsodium-wrappers-sumo` is the documented
  fallback if a noble primitive is ever found lacking.
- **Rust (desktop client & sync runtime):** **RustCrypto** —
  `chacha20poly1305`, `argon2`, `hkdf` + `sha2`, `ed25519-dalek`. Pure-Rust, no C
  toolchain, audited, and — critically — **same-RFC implementations as noble**, so
  the two stacks converge on identical bytes. (`libsodium`/`sodiumoxide` is the
  fallback.)

Both stacks are **pure-implementation, RFC-conformant** (XChaCha20-Poly1305 draft-irtf,
Argon2id RFC 9106, HKDF RFC 5869, Ed25519 RFC 8032). Parity is **guaranteed by the
mandatory KAT vectors in §13**, which both stacks' CI **MUST** pass (architecture §15
"cross-platform crypto parity").

### 9.2 RNG requirements (LOCKED)

- **Rust:** `OsRng` (via `getrandom`) for all nonces, keys, salts, `client_id`,
  recovery codes. Never a userspace PRNG for key material.
- **JS/WASM:** `crypto.getRandomValues`. Never `Math.random`.

---

## 10. Key storage at rest

- The **vault key** and the **Ed25519 device private key**, when unwrapped, live in
  the **OS keychain**: **macOS Keychain / Windows Credential Manager**, accessed via
  the Rust **`keyring`** crate (`keyring-rs`) (or the Tauri `stronghold`/keychain
  plugin) — never the raw file system.
- Keys are **NEVER** written to `localStorage`, `sessionStorage`, IndexedDB, or
  plaintext SQLite. (E2E protects transit/server; local-at-rest is the separate
  concern of architecture §4.7 and is handled by the keychain, not by leaving keys in
  the CRR SQLite file.)
- Unwrapped keys in memory are wrapped in the **`zeroize`** crate's `Zeroizing<..>`
  and are **zeroized on lock / logout / process exit**. This guaranteed zeroization is
  **Rust-side only**.
- **A2 — JS/WASM has no guaranteed memory zeroization (honest statement).** The
  share-viewer runs in a JS engine with a garbage collector: `K_share` and any derived
  key material sit in GC-managed heap that the app **cannot reliably overwrite** —
  there is no portable `zeroize` equivalent, copies may be made by the runtime, and
  reclamation timing is not under our control. The viewer keeps `K_share` in a local
  variable only, never persists it, and **"drops `K_share` on navigation" is
  best-effort** (dropping the reference makes it *eligible* for GC; it does not
  scrub the bytes). We do not claim cryptographic erasure on the JS/WASM stack; the
  mitigation there is minimizing lifetime and never persisting the key (§8), not
  zeroization.

---

## 11. Nonce discipline, versioning, failure handling

### 11.1 Version byte (LOCKED)

Every sealed envelope (standard and committing) starts with a **1-byte version**
(`0x01` for this spec). A decryptor **MUST** reject unknown versions (quarantine, not
crash). This is the forward-agility lever: a future AEAD/KDF migration bumps the
version byte and both stacks branch on it.

**C1 — the version byte is authenticated (LOCKED).** "Reject unknown versions →
quarantine" is **necessary but NOT sufficient** for downgrade resistance: once a
*second valid* version exists, an attacker could flip `vN → v1` (both known) to force
a peer onto a deprecated/weaker construction, and the "unknown version" check never
fires. Therefore the version byte is **also bound as the FIRST AAD field of every
seal** (§5.2) and is covered by the AEAD tag. A flipped version changes the AAD, so
`open()` fails and the item quarantines — downgrade to any prior version is an
authentication failure, not a silently-accepted directive. The plaintext leading byte
is used only to *select which open() to attempt*; the tag is what makes it
authoritative. All §6 version gates likewise trust the authenticated value (§5.4).

### 11.2 RNG (LOCKED)

Restated for emphasis: `OsRng`/`getrandom` (Rust), `crypto.getRandomValues` (JS) for
**every** nonce, key, salt, and ID. 192-bit random nonces per §1.2.

### 11.3 Decrypt-failure handling (LOCKED)

A decrypt failure (bad tag, commitment mismatch, unknown version, malformed
envelope) is **skip-and-flag quarantine, NEVER a halt of the pull cursor.** The
runtime moves the offending item to a local **crypto-quarantine** record (distinct
from the schema-desync `pending_changes` quarantine of architecture §6.3),
increments the pull cursor past it, surfaces a diagnostic, and continues. Rationale:
a single corrupt/tampered blob must not deadlock a device's entire sync. Quarantined
items are retryable (e.g. after a key-state change or vault-key rotation).

---

## 12. Parameter & label registry (single reference table)

| Constant | Value |
|---|---|
| AEAD | XChaCha20-Poly1305 (192-bit nonce) |
| Nonce length | 24 bytes (random, CSPRNG) |
| Tag length | 16 bytes |
| Standard envelope | `0x01 \|\| nonce(24) \|\| ct \|\| tag(16)` |
| Committing envelope | `0x01 \|\| commit(32) \|\| nonce(24) \|\| ct \|\| tag(16)` |
| Audio stream envelope (§1.5) | `LP(wrapped_data_key) \|\| header(24) \|\| seg_0…seg_n`; `header = 0x01 \|\| chunk_size(u32be) \|\| nonce_prefix(19)`; `seg = ct \|\| tag(16)` |
| Audio STREAM primitive | `StreamBE32` over XChaCha20-Poly1305; segment nonce `nonce_prefix(19) \|\| counter(u32be) \|\| last_flag(1)` |
| Audio chunk size | 1 MiB plaintext/segment (v1 constant, in header) |
| Argon2id (client) | m=65536 KiB, t=3, p=4, v=0x13, out=32 |
| Argon2id floor (client) | ≥ m=65536, t=3, p=4, v=0x13, out=32 |
| Argon2id (server verifier) | m=19456 KiB, t=2, p=1, v=0x13, out=32 |
| KDF split (password) | Argon2id → HKDF-SHA256-Expand → `auth_key` / `master_key` (§2.3) |
| Recovery code | 160-bit CSPRNG, base32, 8×4 groups |
| Recovery split (§6.2) | HKDF-SHA256-Expand `yapstack.recovery.v1` L=64 → `recovery_key` = `[0..32]` (wrap, unchanged) / `recovery_auth_key` = `[32..64]` (auth → `/auth/recover`) |
| Signing | Ed25519 (RFC 8032) |
| Fingerprint | base32(SHA-256(pub)[..10]), 4×4 groups |
| `info` yapstack.auth.v1 | `796170737461636b2e617574682e7631` |
| `info` yapstack.master.v1 | `796170737461636b2e6d61737465722e7631` |
| `info` yapstack.commit.v1 | `796170737461636b2e636f6d6d69742e7631` |
| `info` yapstack.recovery.v1 | `796170737461636b2e7265636f766572792e7631` |
| `info` yapstack.devicelist.sign.v1 | `796170737461636b2e6465766963656c6973742e7369676e2e7631` |
| Domain yapstack.changeset.v1 | `796170737461636b2e6368616e67657365742e7631` |
| Domain yapstack.audio.v1 (**RETIRED**, §1.5) | `796170737461636b2e617564696f2e7631` |
| Domain yapstack.audio.stream.v1 | `796170737461636b2e617564696f2e73747265616d2e7631` |
| Wrap domain yapstack.wrap.audio.stream.v1 | `796170737461636b2e777261702e617564696f2e73747265616d2e7631` |
| Domain yapstack.snapshot.v1 (§5.2, shipped) | `796170737461636b2e736e617073686f742e7631` |
| Wrap domain yapstack.wrap.snapshot.v1 (§5.2, shipped) | `796170737461636b2e777261702e736e617073686f742e7631` |
| Domain yapstack.share.v1 | `796170737461636b2e73686172652e7631` |

---

## 13. Known-answer test vectors (KAT)

> **Provenance:** the hex values below were generated and roundtrip-verified with the
> **RustCrypto** reference stack (`argon2` 0.5.3, `chacha20poly1305` 0.10.1,
> `hkdf` 0.12.4, `sha2` 0.10.9) during authoring of this spec — they are **real, not
> placeholder**. Both stacks' CI **MUST** reproduce every value below (the JS/`@noble`
> stack proving parity is a T007 CI bring-up deliverable). All inputs are fixed for
> determinism; production nonces/salts/keys are random per §11.
>
> **C1 regeneration (T006b):** vectors §13.4 and §13.5 were **regenerated** after the
> version byte became the **first AAD field** (§5.2). Both were re-sealed and
> re-opened in-process with the RustCrypto stack (`open()` asserted to succeed) before
> being copied here — they are real, not fabricated. Only `ct||tag` (and the derived
> `sealed`/`committing` blobs and `aad`) changed; keys, nonces, plaintexts, and the
> HKDF commitment are unchanged.

### 13.1 Argon2id client stretch

```
password      (utf8) = "correct horse battery staple"
password      (hex)  = 636f727265637420686f727365206261747465727920737461706c65
salt          (ascii)= "yapstack-kat-salt-0001"
salt          (hex)  = 796170737461636b2d6b61742d73616c742d30303031
params               = Argon2id, m=65536 KiB, t=3, p=4, v=0x13, outlen=32
--- output ---
argon2id_out  (hex)  = 988d57444a7f6d69b1633090d270589b41ed7020809779bc49ecf98d3f714427
```

### 13.2 HKDF-Expand split (auth_key / master_key)

```
prk (= 13.1 out)     = 988d57444a7f6d69b1633090d270589b41ed7020809779bc49ecf98d3f714427
info_auth   (ascii)  = "yapstack.auth.v1"
info_master (ascii)  = "yapstack.master.v1"
--- outputs (HKDF-SHA256 Expand-only, L=32) ---
auth_key             = 49a406dc04cfc8a1be7ad8b26bced86c821af2858cb7f3c50841309ba5d95400
master_key           = 8932d4245ecca12346969e1f6840dd59f61a0209a5814e9866333e8b7768fdfe
```

### 13.3 Server second-hash (verifier)

```
input (= auth_key)   = 49a406dc04cfc8a1be7ad8b26bced86c821af2858cb7f3c50841309ba5d95400
server_salt (ascii)  = "yapstack-srv-salt-000001"
server_salt (hex)    = 796170737461636b2d7372762d73616c742d303030303031
params               = Argon2id, m=19456 KiB, t=2, p=1, v=0x13, outlen=32
--- output ---
stored_verifier      = 474aca96759afc64ab67eb261cb8cf315d73ebf433aed3f11dccb5c6c3fc040d
```

### 13.4 XChaCha20-Poly1305 seal/open with AAD (standard envelope)

> **C1 applied:** the 1-byte `version` (`0x01`) is the **FIRST AAD field**. Vectors
> below were regenerated after that change and are real, roundtrip-verified.

```
data_key             = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
nonce (24)           = a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7
plaintext (utf8)     = "the quick brown fox"
plaintext (hex)      = 74686520717569636b2062726f776e20666f78
--- AAD (changeset), LP(version, domain, tenant, client, client_seq, schema_v, engine_v) ---
  version (1)        = 01                 (C1: FIRST AAD field)
  domain             = "yapstack.changeset.v1"
  tenant_id (16)     = 11111111111111111111111111111111
  client_id (16)     = 22222222222222222222222222222222
  client_seq (u64be) = 000000000000002a   (= 42)
  schema_version(u32)= 00000007           (= 7)
  engine_version(u32)= 00003e83           (= 16003, i.e. cr-sqlite 0.16.3)
  aad (hex)          = 0000000101
                       00000015796170737461636b2e6368616e67657365742e7631
                       0000001011111111111111111111111111111111
                       0000001022222222222222222222222222222222
                       00000008000000000000002a
                       0000000400000007
                       0000000400003e83
  (aad concatenated) = 000000010100000015796170737461636b2e6368616e67657365742e76310000001011111111111111111111111111111111000000102222222222222222222222222222222200000008000000000000002a00000004000000070000000400003e83
--- outputs ---
ct||tag              = 31846f3dc628cdcf0a4f4ffb1e47cde05dc5e77a09e2dbf8629e1577b5f46df1657a3c
sealed (0x01||nonce||ct||tag) =
  01a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b731846f3dc628cdcf0a4f4ffb1e47cde05dc5e77a09e2dbf8629e1577b5f46df1657a3c
open with same key+nonce+aad -> plaintext (verified)
```

### 13.5 Committing seal roundtrip (wrapped-key / share)

```
K_root               = 404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f
nonce (24)           = 101112131415161718191a1b1c1d1e1f2021222324252627
wrapped plaintext(32)= 808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f
aad = LP(version, "yapstack.share.v1", "share-abc123")   (C1: version FIRST)
  version (1)        = 01
  aad (hex)          = 000000010100000011796170737461636b2e73686172652e76310000000c73686172652d616263313233
--- HKDF-SHA256 Extract+Expand(salt=nonce, ikm=K_root, info="yapstack.commit.v1", L=64) ---
okm (64)             = 33a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb4a89cea795fa8fea5176d82f41d715216fa5bef84a4ca710c2db1dd8dfd19f1b
commitment (32)      = 33a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb
k_aead (32)          = 4a89cea795fa8fea5176d82f41d715216fa5bef84a4ca710c2db1dd8dfd19f1b
--- outputs ---
ct||tag              = 358375100371384674ba02e3be7c96f582e12a620aa52a4266080d56491b94f513a00e7bdb093a3ba5bd8c9409df26e0
committing (0x01||commit||nonce||ct||tag) =
  0133a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb101112131415161718191a1b1c1d1e1f2021222324252627358375100371384674ba02e3be7c96f582e12a620aa52a4266080d56491b94f513a00e7bdb093a3ba5bd8c9409df26e0
open: recompute okm from K_root+nonce, constant-time check commitment, AEAD-open -> plaintext (verified)
```

> **Note on the commitment/k_aead unchanged, ct||tag changed:** the HKDF
> `okm`/`commitment`/`k_aead` depend only on `(K_root, nonce)`, so C1 does not move
> them; only `ct||tag` changes because the AAD now leads with the version byte.

### 13.6 Ed25519 device-list signature — Rust roundtrip in CI; pinned vector deferred

> The signing seed derivation (§7.2), canonical roster encoding (§7.3), and an
> Ed25519 sign/verify roundtrip run in CI today
> (`crates/yapstack-crypto/tests/kat.rs::kat_13_6_ed25519_roster_signature`,
> fixed message, tamper rejection asserted). A pinned fixed-input hex vector
> cross-checked against `@noble/curves` (JS) is deferred to the share-viewer
> tranche, alongside the JS stack itself. Deterministic Ed25519 (RFC 8032) makes
> this a stable KAT when generated.

### 13.7 Recovery split — `recovery_key` / `recovery_auth_key` (§6.2)

> **Real, roundtrip-verified.** Generated with the crate's own
> `yapstack_crypto::hkdf::expand` primitive (RFC 5869 HKDF-Expand-SHA256, the exact
> code production uses); block 1 was asserted byte-equal to
> `yapstack_crypto::kdf::recovery_key(recovery_bytes)` in the same run, proving the
> `L=64` expansion does not disturb the existing vault wrap. Both stacks' CI **MUST**
> reproduce these bytes.

```
recovery_bytes (20)  = 000102030405060708090a0b0c0d0e0f10111213
info (ascii)         = "yapstack.recovery.v1"
info (hex)           = 796170737461636b2e7265636f766572792e7631
--- HKDF-SHA256 Expand-only, L=64 ---
okm (64)             = 39684df2364fab3f87645c80ad59c9eec298ea212c84f3fbb7c49f303f6557c5
                       1e5ad1dbc106f5791bb28d514097f462fece2837b3b5a02b6708364d6655db8b
recovery_key   [0..32]= 39684df2364fab3f87645c80ad59c9eec298ea212c84f3fbb7c49f303f6557c5
recovery_auth_key[32..64]= 1e5ad1dbc106f5791bb28d514097f462fece2837b3b5a02b6708364d6655db8b
```

> **Note:** `recovery_key` above is byte-identical to what `HKDF-Expand(..., L=32)`
> produces (block 1 = `T(1)`), so `wrapped_vault_key_recovery` is unchanged by this
> ratification. `recovery_auth_key` (block 2 = `T(2)`) is the value the client sends to
> `POST /auth/recover`; the server second-hashes it (§3.1) into `recovery_verifier`.

### 13.8 Audio stream envelope (§1.5) — GENERATED

> **Real, roundtrip-verified.** No audio KAT existed before this amendment (§13.4 is
> changeset-domain and proves nothing about audio). These vectors were **generated** with
> this crate's own `yapstack_crypto::audio_stream` primitives (the exact code production
> uses) and asserted to reproduce + round-trip in
> `crates/yapstack-crypto/tests/audio_stream.rs`. All inputs are fixed for determinism;
> production data keys / nonce prefixes are random per §11. The JS/`@noble` parity vector
> is a Phase-4 share-viewer deliverable (the viewer does not play audio in v1).

**Segment vector** — 3 segments at `chunk_size = 8` (a short last segment):

```
data_key             = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
nonce_prefix (19)    = a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2
chunk_size (u32be)   = 00000008
plaintext (utf8)     = "the quick brown fox"   (19 bytes ⇒ segments 8 / 8 / 3)
plaintext (hex)      = 74686520717569636b2062726f776e20666f78
--- header (version || chunk_size || nonce_prefix), authenticated as AAD on every segment ---
header (24)          = 0100000008a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2
--- outputs (StreamBE32 / XChaCha20-Poly1305; each seg = ct || tag(16)) ---
header||segments      = 0100000008a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2
                        a60589414748d24e8126f11ca3b289b97fbacba45e5cd173   (seg0: 8ct+16tag)
                        5522e597d0230d16d836467d7038fe914a0d60ef3870bcc0   (seg1: 8ct+16tag)
                        2baff86b20d167fac830f13e3d0b35e3a294b0             (seg2: 3ct+16tag)
open with same data_key → plaintext (verified); drop seg2 → open fails (truncation);
swap seg0/seg1 → open fails (reorder)
```

**Data-key wrap vector** — committing envelope with the identity AAD (§4.2 / D6):

```
vault_key            = 404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f
audio_data_key (pt)  = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
wrap_nonce (24)      = 101112131415161718191a1b1c1d1e1f2021222324252627
identity tuple       = tenant_id=11..11(16) session_id=22..22(16) part_id=33..33(16) epoch=0
aad = LP(version, "yapstack.wrap.audio.stream.v1", tenant_id, session_id, part_id, epoch_u32)
--- output (committing: 0x01 || commit(32) || nonce(24) || ct(32) || tag(16) = 105 bytes) ---
wrapped_data_key     = 0133a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb
                        101112131415161718191a1b1c1d1e1f2021222324252627
                        b503f59083f1b8c6f43a82633efc16750261aae28a25aac2e6888dd6c99b1475
                        08f87190c05d4310c20760a359fd19fc
unwrap under the same identity → audio_data_key (verified);
unwrap under a different part_id OR a different epoch_u32 → fails (identity/anti-rollback)
```

> **Note:** the `commitment`/`nonce`/`k_aead` of the wrap depend only on `(vault_key,
> wrap_nonce)`, so this vector's commitment (`33a7b8…cfbb`) matches §13.5's — only the
> `ct||tag` differs (different plaintext key and audio wrap AAD).

---

## 14. Compliance checklist for implementers (both stacks)

- [ ] XChaCha20-Poly1305 only; AES-GCM absent from the multi-writer path.
- [ ] Argon2id params are compile-time constants; no server-supplied KDF params; floor check present.
- [ ] Single Argon2 pass + HKDF split with the exact `info` strings in §12.
- [ ] Server stores `Argon2id(auth_key, server_salt)`, never `auth_key`, never the password.
- [ ] Committing envelope used for every wrapped key and every share; standard envelope for changesets.
- [ ] Audio uses the §1.5 STREAM envelope ONLY (`yapstack.audio.stream.v1`); the whole-blob `yapstack.audio.v1` domain is retired/unused; the per-blob data key is wrapped under `yapstack.wrap.audio.stream.v1` binding `(tenant_id, session_id, part_id, epoch_u32)`; the clear header is the per-segment AAD; STREAM is the maintained crate (`aead::stream`), never hand-rolled.
- [ ] AAD is `LP(...)` per §5 with the exact fields per surface; **`version` byte is the FIRST AAD field on every surface** (C1); `client_seq` (not server seq) bound.
- [ ] §6 version/quarantine gates key off the **AAD-authenticated** `schema_version`/`engine_version`, never the server's plaintext metadata columns (C4).
- [ ] Recovery code 160-bit CSPRNG, base32, forced capture; HKDF (no Argon2).
- [ ] Fresh UUIDv4 `client_id`; Ed25519 roster signed by vault-derived key; monotonic `counter` + `epoch` anti-rollback.
- [ ] Vault key + device key in OS keychain; `zeroize` on lock; never localStorage/plaintext SQLite.
- [ ] Version byte on every envelope AND authenticated as the first AAD field (C1); decrypt failure → quarantine, never halt the cursor.
- [ ] Client caches its own `salt_enc`; alerts on server-supplied mismatch for known devices (C3).
- [ ] First device self-enrolls (signed roster, epoch 0); new devices re-anchored via approval ceremony with out-of-band roster-fingerprint + epoch check (C2).
- [ ] All §13 KAT vectors reproduced in CI on both stacks (parity gate).
```
