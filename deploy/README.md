<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# YapStack relay — operator quick reference

This is the **operator runbook** for the self-hosted sync relay defined by
`docker-compose.yml` in this directory. It is a companion to the full narrative in
[`../docs/self-hosting.md`](../docs/self-hosting.md) (install, RLS/role hardening, reverse
proxy/TLS, two-device onboarding); read that first. This file covers the day-two
operational notes: **keeping the host awake**, the **port/env knobs**, and a **one-glance
backup note**.

The relay is a **blind store** — ciphertext + opaque metadata only, no CRDT engine, no
outbound calls except to your own storage/database, no admin key. Self-host always
resolves to the maximum tier (`AllowAll`).

## Start / stop / status

```sh
cd deploy
cp .env.example .env        # set strong secrets first (see below)
docker compose up -d --build
docker compose ps           # all services healthy?
docker compose logs -f server
curl -s http://localhost:8080/health   # {"status":"ok"}
```

## Keep the host awake

The relay is only reachable while its host machine is **actually running**. On a Mac
serving as an always-on relay this bites in a specific way: closing the lid
(**clamshell**) or letting the machine idle into sleep **suspends Docker and drops every
in-flight connection** — clients then show "relay unreachable" even though nothing is
misconfigured. A host that sleeps on lid-close will silently stall sync for every
device until it wakes.

A relay host must be configured to **never sleep**:

- **macOS (recommended for a dedicated relay):** run under `caffeinate` or disable idle
  sleep outright.
  ```sh
  # Keep the system (and disk) awake indefinitely while the relay should serve.
  sudo pmset -a disablesleep 1        # never idle-sleep, even in clamshell (lid closed)
  sudo pmset -a sleep 0               # belt-and-suspenders: no system sleep
  # Or, non-persistently, hold wake for as long as this process lives:
  #   caffeinate -s docker compose up
  ```
  `disablesleep 1` is the one that matters for a **lidded** Mac mini/laptop relay: without
  it, clamshell mode sleeps regardless of `sleep 0`. Verify with `pmset -g`.
  To restore normal power behaviour: `sudo pmset -a disablesleep 0`.
- **Linux:** mask the sleep targets on a headless server —
  `sudo systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target`.
- Do **not** rely on Docker's `restart: unless-stopped` to cover this. Restart policy
  resumes containers after a **daemon** restart; it does nothing while the **host** is
  asleep — the machine has to stay powered and awake.

Pair this with a reverse proxy + TLS (see the self-hosting doc) so a stable public
hostname survives the host's network hiccups.

## Port / env knobs

All configuration flows through the one `deploy/.env` (see `.env.example`). The relay
process itself reads a rendered TOML (`entrypoint.sh` writes it from these vars); there is
deliberately **no `[limits]` section and no admin key**.

| Env var | Default | What it controls |
|---|---|---|
| `YAPSTACK_HTTP_PORT` | `8080` | Host port published for the relay HTTP API. |
| `YAPSTACK_BIND_ADDR` | `0.0.0.0:8080` | In-container bind address the server listens on. |
| `MINIO_API_PORT` | `9000` | Host port for the MinIO S3 API (client blob upload/download). |
| `MINIO_CONSOLE_PORT` | `9001` | Host port for the MinIO web console. |
| `MINIO_PUBLIC_ENDPOINT` | `http://localhost:9000` | Public URL baked into presigned audio/snapshot URLs — set to your TLS storage hostname behind a proxy. |
| `MINIO_ENDPOINT` | `http://minio:9000` | In-network endpoint the server signs against. |
| `MINIO_BUCKET` | `yapstack` | Bucket holding encrypted audio + bootstrap snapshots. |
| `DATABASE_URL` | owner DSN | Postgres connection. Unset = the owner DSN compose builds from `POSTGRES_*`; set it in `.env` to serve as the non-owner `yapstack_app` role (see self-hosting doc). |
| `YAPSTACK_GC_ENABLED` | `true` | Audio-blob GC master switch (`1`/`true`/`on` vs `0`/`false`/`off`). |
| `YAPSTACK_GC_INTERVAL_SECS` | `86400` | Seconds between GC sweeps (one also runs shortly after boot). |
| `YAPSTACK_GC_GRACE_SECS` | `604800` | How long a blob must be unreferenced before GC may delete it. |
| `JWT_SECRET` | *required* | Signs session tokens. `openssl rand -hex 32`. |
| `SERVER_PEPPER` | *required* | Auth-verifier pepper. `openssl rand -hex 32`. |
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | *required* | Postgres owner credentials + database name. |
| `YAPSTACK_APP_PASSWORD` | *required* | Password for the non-owner serving role. |
| `MINIO_ROOT_USER` / `MINIO_ROOT_PASSWORD` | *required* | MinIO admin credentials (also the S3 signing key). |

Changing a port or endpoint is a `.env` edit + `docker compose up -d` (recreates only the
affected service). Changing `MINIO_PUBLIC_ENDPOINT` requires a server restart so new
presigns carry the new hostname.

## Backup, in one paragraph

Back up **two stores together, or neither**: the **Postgres volume** (`postgres-data`) is
the entire encrypted corpus + wrapped keys + auth verifiers, and the **MinIO bucket**
(`minio-data`) holds the encrypted audio and bootstrap snapshots — a dump of one without a
matching copy of the other is inconsistent. Both are **already ciphertext**, so the
relay's backups can never expose note content; but the actual decryption keys live **only
on the clients** (each user's OS keychain / recovery code), never on the server — so a
relay restore brings back everyone's *encrypted* data, and each device re-derives its keys
locally. Losing both a user's password **and** their recovery code is unrecoverable by
design (true E2E; no server-side reset). See the full dump/mirror/PITR commands and the
monthly restore drill in [`../docs/self-hosting.md`](../docs/self-hosting.md#backup-runbook).
