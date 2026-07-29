# YapStack vendor patches — cr-sqlite

This tree is a vendored copy of vlcn-io/cr-sqlite (imported in commit `ad9233a`,
built standalone against pinned `nightly-2023-10-05` via
`crates/yapstack-sync/build.rs`). We keep local, deliberately-scoped patches here
rather than forking upstream. Each patched site is also marked in-code with a
`YAPSTACK VENDOR PATCH` comment block. Keep patches conservative — no std/library
APIs newer than the pinned nightly toolchain.

## Patches

### 1. `sqlite3_value_text` / `sqlite3_value_blob` API call-order (R7.1)

- File: `core/rs/sqlite-rs-embedded/sqlite3_capi/src/capi.rs`
- Functions: `value_text`, `value_blob`
- Problem: both fetched the byte length via `sqlite3_value_bytes()` **before**
  calling `sqlite3_value_text()` / `sqlite3_value_blob()`. The SQLite C API
  requires the text/blob accessor to be called first; `sqlite3_value_bytes()`
  reports the length that results from that type conversion, so reading it first
  can yield a stale/incorrect length and build a slice that over- or under-runs
  the returned buffer.
- Fix: call the text/blob accessor first, then measure with
  `sqlite3_value_bytes()`. No behavioral change beyond correct ordering.

### 2. NUL-terminate table/schema names crossing the FFI boundary (R7.1)

- File: `core/rs/core/src/lib.rs`
- Functions: `x_crsql_as_crr`, `x_crsql_commit_alter`
- Problem: these passed `&str::as_ptr()` (from `value_text`, not NUL-terminated)
  into `crsql_create_crr` / `crsql_compact_post_alter`, which re-derive the
  strings via `CStr::from_ptr`. `CStr::from_ptr` reads bytes until it finds a
  NUL, so C read whatever adjacent heap followed the identifier — layout-
  dependent behavior. On the R7 binary this deterministically produced a
  spurious `SQLITE_NOMEM` from `crsql_as_crr('sessions')` (invalid UTF-8 in the
  over-read bytes -> `to_str()` Err -> `ResultCode::NOMEM`), aborting
  `cargo test --features sync`. The same path runs in production `perform_cutover`
  (sync-enable) on every user table.
- Fix: copy each name into an owned, NUL-terminated `alloc::ffi::CString` and
  pass that pointer across the FFI boundary. Interior-NUL names (impossible for
  real SQL identifiers) are rejected with a clear error instead of truncating.

## Investigated — NO patch needed

### `crsql_commit_alter` DOES regenerate the update trigger (2026-07-29)

Investigated while root-causing the live `expected 29 values, got 27` failure on
every direct `UPDATE segments` (owner's Windows device). Suspicion was that
`crsql_commit_alter` had failed to recreate `segments__crsql_utrig` at the new
column arity. It does not fail. Verified end-to-end by
`crates/yapstack-sync/tests/trigger_arity.rs::wrapped_alter_leaves_triggers_consistent`,
and traceable in this tree:

- `core/rs/core/src/lib.rs:645` — `crsql_begin_alter` drops the three crsql
  triggers (`teardown.rs:21-55`), so `is_crr` (`is_crr.rs:10-26`, which probes
  `<table>__crsql_itrig`) reports **false** inside the dance.
- `core/rs/core/src/lib.rs:689-707` — `crsql_commit_alter` then runs
  `crsql_compact_post_alter` and `crsql_create_crr`.
- `core/rs/core/src/create_crr.rs:25-37` — because `is_crr` is false, the early
  return is skipped; `pull_table_info` re-reads the CURRENT shape and
  `create_triggers` (`triggers.rs:12-20`) re-emits all three triggers. The
  `CREATE TRIGGER IF NOT EXISTS` in `triggers.rs` is safe precisely because
  `begin_alter` dropped them first.

The real fault was OUR side: a bare `ALTER TABLE … ADD COLUMN` outside the dance
(cr-sqlite accepts it) changes the live shape without regenerating the trigger,
while `x_crsql_after_update` (`local_writes/after_update.rs:43-56`) re-derives the
expected arity from the live shape at call time. Repaired in
`yapstack_sync::schema::heal_stale_crr_triggers`, which re-runs the dance with no
schema change. Two vendor facts make that repair state-preserving and are pinned
by tests: the clock/pks tables are `CREATE TABLE IF NOT EXISTS`
(`bootstrap.rs:195-235`) and are only dropped when the PK set changed
(`alter.rs:65-71`); and the post-alter backfill stamps new clock rows with
`crsql_db_version()`, not `crsql_next_db_version()` (`backfill.rs:107-117`), so it
never bumps `db_version`.

## Regression coverage

The R7-branch tests
`db_service::tests::crr_multi_statement_*` and `crr_prepared_alter_*`
(in `apps/desktop/src-tauri/src/db_service.rs`) exercise the `crsql_as_crr`
path end-to-end and were the failing baseline these patches fix.
