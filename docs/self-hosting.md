<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Self-hosting the YapStack sync relay

YapStack sync is **end-to-end encrypted**. The relay is a **blind store**: it holds
only ciphertext and opaque metadata, runs no CRDT engine, decrypts nothing, and its
only outbound calls are to the object storage and database **you** configure (one
metadata-only HEAD per upload check; no telemetry, no check-ins, no kill switch).
There is **no admin key** and **no feature gating** in a self-host deployment —
self-host is always the maximum tier (entitlements resolve to `AllowAll`). This is
licensed **AGPL-3.0-only**; a fork of the server (or any crate) must share back under
AGPL.

The stack is four containers (one is a one-shot bucket initializer) wired by one `.env`:

| Service | Role |
|---|---|
| `server` (`yapstack-server`) | the blind relay: auth, changeset store/fan-out, audio + snapshot presigning |
| `postgres:16` | metadata + **ciphertext** blobs, tenant isolation via RLS |
| `minio` | S3-compatible object storage for **encrypted** audio + the R2 bootstrap snapshot |

## Install

```sh
cd deploy
cp .env.example .env
# Edit .env: set strong secrets. Generate them, e.g.:
#   openssl rand -hex 32   # JWT_SECRET
#   openssl rand -hex 32   # SERVER_PEPPER
#   openssl rand -hex 24   # POSTGRES_PASSWORD / YAPSTACK_APP_PASSWORD / MINIO_ROOT_PASSWORD
docker compose up -d --build
```

The server serves on `http://localhost:8080` (`YAPSTACK_HTTP_PORT`). Check health:

```sh
curl -s http://localhost:8080/health      # {"status":"ok"}
curl -s http://localhost:8080/sync/info   # protocol/min-client/engine versions; NO billing_url
```

In the desktop app, open **Settings → Sync**, choose *self-host*, and paste your relay
URL (behind TLS in production, see below).

## What the relay never sees

- Note bodies, segments, sessions, folders, chat, dictation → **ciphertext only**.
- Audio WAV/MP3 and the bootstrap snapshot → **ciphertext only**, in MinIO. Bytes flow
  **client ↔ MinIO** directly via presigned URLs; they never pass through the relay,
  which only computes the signatures (pure local HMAC).
- Email + auth verifier + KDF salts + wrapped vault keys → yes, but the wrapped keys are
  inert without the user's password/recovery code. **Lost password + lost recovery code
  = unrecoverable data** — there is no server-side reset (true E2E).

## The `yapstack_app` (non-owner) role — RLS note

Every tenant-scoped table is `ENABLE` + `FORCE ROW LEVEL SECURITY` with a fail-closed
predicate keyed on a transaction-local `app.tenant_id` guard (see
`crates/yapstack-server/migrations/0001_initial.sql`). `FORCE` RLS applies even to the
table owner **but not to a Postgres superuser** — a superuser (or any role with the
`BYPASSRLS` attribute) bypasses row security entirely. So tenant isolation does **not**
hold "regardless of role": **it holds only when the serving connection is a non-owner,
non-superuser role.** If the server connects as a superuser, the RLS policies are inert
and the only isolation left is the hand-written `workspace_id = $tenant` predicates in
the queries. That is why the serving connection must be `yapstack_app`.

The migrations create that **non-owner** role, `yapstack_app`, and grant it only
`SELECT/INSERT/UPDATE/DELETE` on the tenant tables (plus `EXECUTE` on the SECURITY
DEFINER `yapstack_lookup_login` used by the pre-tenant login/recover paths). The DB init
script (`deploy/postgres-init/00-yapstack-roles.sh`) creates it with `LOGIN` + the
`YAPSTACK_APP_PASSWORD` password so the **serving** connection runs as this non-owner —
it can never own tables or bypass RLS.

Migrations are the **only** step that needs `CREATE` on schema `public`, so they run as
the owner. The serving role never runs DDL: `db::migrate` reads the applied-migration
watermark and **skips** the migrator when the schema is already current, so booting as
`yapstack_app` needs no schema privileges.

- **Default compose:** a one-shot `migrate` service applies the schema as the DB
  **owner** (`POSTGRES_USER`), then the long-running `server` connects as the non-owner
  `yapstack_app` role. This is the zero-config path **and** the hardened posture — no
  extra steps.
- **Managed Postgres caveat:** the pre-tenant login lookup runs through a SECURITY
  DEFINER function owned by the migration owner. This bypasses RLS correctly when that
  owner is a superuser (the compose default) or any `BYPASSRLS` role. On a managed
  provider where even the migration owner is a plain non-superuser table owner subject to
  `FORCE` RLS, grant that owner `BYPASSRLS` (or run migrations as a role that has it) so
  the login lookup can bridge the pre-tenant read.
- **Future schema upgrades:** re-run the one-shot migrate step as the owner
  (`docker compose run --rm migrate`, or a fresh `docker compose up` which runs it
  automatically), then restart the server.

## Reverse proxy (TLS)

Terminate TLS at a proxy in front of the relay; never expose plaintext HTTP publicly.
Presigned URLs and bearer tokens are capabilities — TLS protects them in transit.

Caddy example:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8080
}
storage.example.com {
    reverse_proxy 127.0.0.1:9000
}
```

Then set `MINIO_PUBLIC_ENDPOINT=https://storage.example.com` in `.env` so presigned
audio/snapshot URLs point clients at the public MinIO hostname, and restart. Keep
request-body limits generous on the `/audio/*` and `/snapshot/*` paths — clients upload
**directly to MinIO**, but the presign POSTs are small.

**Rate limiting behind the proxy.** The relay throttles `login`, `signup`, and `push`
per client IP. Behind TLS termination it only sees the proxy's address, so it trusts the
`X-Forwarded-For` header **only** when the connecting peer is a configured trusted proxy
— otherwise it fails **closed** (ignores XFF and keys on the peer, so a spoofed header
can't rotate into fresh buckets). Set `YAPSTACK_TRUSTED_PROXIES` in `.env` to the source
IP the relay sees your proxy connect from (with the compose stack, that's the Docker
bridge gateway, typically `172.x.0.1` — `docker network inspect` shows it). Leave it
unset only if the relay is exposed directly with no proxy in front.

**Optional signup gate.** To keep a private relay from accepting open sign-ups, set
`YAPSTACK_SIGNUP_INVITE` to a shared secret. When set, `POST /auth/signup` requires a
matching `X-YapStack-Invite` header (constant-time compared); unset leaves signup open
(the default). Login, refresh, and recovery are unaffected.

Notes on scale: SSE live-push (`GET /sync/stream`) is best-effort; **pull is the source
of truth**. Running more than one app server requires a `LISTEN/NOTIFY` (or Redis) bus
for cross-server SSE fan-out; a single relay needs none.

## Backup runbook

Two independent stores must be backed up **together** — a Postgres snapshot without the
matching MinIO objects (or vice-versa) is inconsistent.

1. **Postgres** — logical dump (metadata + ciphertext blobs + wrapped keys):
   ```sh
   docker compose exec -T postgres pg_dump -U "$POSTGRES_USER" -Fc "$POSTGRES_DB" > yapstack-$(date +%F).dump
   ```
   For the disaster case, enable **PITR** (WAL archiving) on Postgres. PITR restores
   *data*, not schema — **additive-only migrations** are the actual safety lever against
   a bad migration.
2. **MinIO** (encrypted audio + snapshots):
   ```sh
   mc mirror --overwrite yap/"$MINIO_BUCKET" ./minio-backup/
   ```
3. **Restore drill (do this monthly):** restore both into a scratch stack and confirm a
   client can log in, pull, and decrypt. A backup you have never restored is a guess.

All backups are **already encrypted** at the content layer, but still store them
securely: they contain the wrapped vault keys and auth verifiers.

## Two-device onboarding

Enabling sync on each device is the same single flow: **Settings → Sync → enable**.
Every device captures its local library and merges what the relay holds. Because all
synced records use random unique IDs, two **independently-grown** libraries merge as a
lossless union — nothing is overwritten.

**One rule: never copy a YapStack library between machines by hand** (e.g. copying the
database file to a second computer) and then sync both. Two copies of the *same*
library edited on both sides share record IDs, and the merge resolves each conflicting
field to one winner — the loser's edit is dropped silently. Sign the second machine in
and let sync populate it instead.

A snapshot-based **seed/join** flow (compact bootstrap for very large libraries, with
surfaced conflict review) is designed and partially built but **not yet available in
the app** — current builds always use the capture-and-merge path above.
