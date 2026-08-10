// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres pool, migrations, and the RLS pooling guard.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Connect the pool. The connection string SHOULD authenticate as the non-owner
/// `yapstack_app` role so RLS can never be bypassed (see `migrations/0001_initial.sql`).
///
/// # Errors
/// Returns a `sqlx::Error` if the pool cannot be established.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(16)
        .connect(database_url)
        .await
}

/// Run embedded migrations (compiled in from `migrations/`, no DB needed at build).
///
/// Migrations are an OWNER-only, one-time step (see `docs/self-hosting.md` and the
/// compose `migrate` service). The non-owner serving role `yapstack_app` has only
/// USAGE on schema `public` — no CREATE — but sqlx's `Migrator::run` UNCONDITIONALLY
/// issues `CREATE TABLE IF NOT EXISTS _sqlx_migrations` before it reads applied
/// versions, which Postgres rejects for that role even when every migration is already
/// applied. So when the migrations table already exists and its highest applied version
/// is at or beyond the highest embedded version, skip the Migrator entirely: the serving
/// role then boots without running any DDL. The two probe queries are plain reads the
/// app role is granted — `to_regclass` is a catalog lookup, and `_sqlx_migrations`
/// carries an explicit SELECT grant (migration `0007`).
///
/// # Errors
/// Returns a migration error if any statement fails.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let migrator = sqlx::migrate!("./migrations");

    if let Some(embedded_max) = migrator.iter().map(|m| m.version).max() {
        let table_exists: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations')::text")
                .fetch_one(pool)
                .await?;
        if table_exists.is_some() {
            let applied_max: Option<i64> =
                sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
                    .fetch_one(pool)
                    .await?;
            if applied_max.is_some_and(|a| a >= embedded_max) {
                // Intentional: skipping the Migrator here bypasses sqlx's per-migration
                // checksum verification. That verification requires DDL privileges the
                // non-owner serving role does not have; the owner's migrate one-shot is
                // the checksum-verifying path, so this early return is safe.
                return Ok(());
            }
        }
    }

    migrator.run(pool).await
}

/// Begin a transaction with the RLS guard set: `app.tenant_id` is bound
/// TRANSACTION-LOCAL (the `true` third arg to `set_config`), so it cannot leak across
/// pooled connections. Reading it back with `current_setting('app.tenant_id', true)`
/// returns NULL when unset, which the RLS predicates treat as "deny all" (fail-closed).
///
/// `tenant` MUST come from a validated access token, never from the request body.
///
/// # Errors
/// Returns a `sqlx::Error` if the transaction cannot start or the guard cannot be set.
pub async fn begin_tenant_tx(
    pool: &PgPool,
    tenant: Uuid,
) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
