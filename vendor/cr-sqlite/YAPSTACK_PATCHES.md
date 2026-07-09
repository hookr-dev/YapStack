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

## Regression coverage

The R7-branch tests
`db_service::tests::crr_multi_statement_*` and `crr_prepared_alter_*`
(in `apps/desktop/src-tauri/src/db_service.rs`) exercise the `crsql_as_crr`
path end-to-end and were the failing baseline these patches fix.
