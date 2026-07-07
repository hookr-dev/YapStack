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
/// # Errors
/// Returns a migration error if any statement fails.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
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
