// SPDX-License-Identifier: AGPL-3.0-only
//! Static-link the pinned cr-sqlite v0.16.3 CRR engine INTO this crate (R6).
//!
//! There is no published `crsqlite` Rust crate; upstream ships a loadable
//! extension whose Rust half is a `#![no_std]` `staticlib` requiring the pinned
//! `nightly-2023-10-05` toolchain. We therefore:
//!
//!   1. Build the vendored Rust bundle (`crsql_bundle_static`) STANDALONE with the
//!      pinned nightly (its own `rust-toolchain.toml`) into `OUT_DIR`, producing
//!      `libcrsql_bundle_static.a` on unix (macOS/linux) or `crsql_bundle_static.lib`
//!      on windows-msvc. It is excluded from this workspace's stable
//!      graph (root `Cargo.toml` `exclude`), so it never contaminates the build.
//!   2. Compile the three vendored C shims (`crsqlite.c`, `changes-vtab.c`,
//!      `ext-data.c`) with `-DSQLITE_CORE` so they bind DIRECTLY to the SQLite
//!      symbols that `rusqlite`'s bundled `libsqlite3-sys` exports (NOT the
//!      loadable-extension API — the desktop stack disables `load_extension`).
//!   3. Link both into `yapstack-sync`. Registration at runtime is via
//!      `sqlite3_auto_extension(sqlite3_crsqlite_init)` (see `crsqlite.rs`).
//!
//! This static path was UNPROVEN by the T003 spike (which used the prebuilt
//! `.dylib`); it is proven here and by the crate's tests, which register a CRR and
//! call `crsql_as_crr` / `crsql_changes` through a `rusqlite` connection.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned cr-sqlite release (matches `vendor/cr-sqlite`, tag v0.16.3).
const CRSQLITE_COMMIT_SHA: &str = "0d62b52b4662ee1a762c9fd9264d48a91ab8df83";

/// `Path::canonicalize` on Windows returns an extended-length `\\?\` path that
/// MSVC's `cl.exe` cannot open (it fails with `C1083: Cannot open source file`
/// when such a path is handed to the C compiler via the `cc` crate). Strip the
/// `\\?\` verbatim prefix so downstream tooling gets a plain absolute path.
/// No-op on non-Windows (macOS/Linux paths are already plain).
fn plain_canonical(p: PathBuf) -> std::io::Result<PathBuf> {
    let canonical = p.canonicalize()?;
    #[cfg(windows)]
    {
        let s = canonical.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // repo_root/vendor/cr-sqlite/core
    let vendor_core = plain_canonical(manifest.join("../../vendor/cr-sqlite/core"))
        .expect("vendored cr-sqlite/core must exist");

    let bundle_static = vendor_core.join("rs/bundle_static");
    let c_src = vendor_core.join("src");
    let sqlite_hdr = vendor_core.join("src/sqlite");

    // Rebuild triggers.
    println!("cargo:rerun-if-changed=build.rs");
    for f in [
        "crsqlite.c",
        "crsqlite.h",
        "changes-vtab.c",
        "changes-vtab.h",
        "ext-data.c",
        "ext-data.h",
        "ext.h",
        "consts.h",
        "util.h",
        "rust.h",
    ] {
        println!("cargo:rerun-if-changed={}", c_src.join(f).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        bundle_static.join("src/lib.rs").display()
    );

    // --- 1. Build the vendored Rust static bundle with the pinned nightly. ---
    let cr_target = out_dir.join("crbuild");
    let status = Command::new("cargo")
        // The bundle's own rust-toolchain.toml selects nightly-2023-10-05. Clear
        // any inherited toolchain override so rustup honours the vendored pin, and
        // clear RUSTFLAGS so the outer workspace's flags never reach the old nightly.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CRSQLITE_COMMIT_SHA", CRSQLITE_COMMIT_SHA)
        .current_dir(&bundle_static)
        .args([
            "build",
            "--release",
            "--features",
            "static,omit_load_extension",
            "--target-dir",
        ])
        .arg(&cr_target)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn cargo for the vendored cr-sqlite static bundle \
                 (is rustup + the pinned nightly-2023-10-05 toolchain installed?): {e}"
            )
        });
    assert!(
        status.success(),
        "building the vendored cr-sqlite Rust static bundle failed. This crate \
         static-links cr-sqlite v0.16.3, whose Rust half needs the pinned \
         nightly-2023-10-05 toolchain (declared in \
         vendor/cr-sqlite/core/rs/bundle_static/rust-toolchain.toml). Install it: \
         `rustup toolchain install nightly-2023-10-05`."
    );

    let rust_lib_dir = cr_target.join("release");
    // The nested `cargo build` (no `--target`) emits the staticlib for the HOST
    // toolchain's default target, which — in this repo's non-cross-compiling CI —
    // matches the target `yapstack-sync` itself is being built for. Predict the
    // produced filename from THIS crate's target so the existence check is
    // platform-correct: MSVC has no `lib` prefix and uses the `.lib` extension
    // (`crsql_bundle_static.lib`), while unix (macOS/linux) uses the GNU archive
    // form (`libcrsql_bundle_static.a`). The `cargo:rustc-link-lib=static=` LINK
    // directive below stays the same logical name on both platforms — cargo/rustc
    // reattach the platform prefix/extension for linking.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let static_lib_file = if target_os == "windows" && target_env == "msvc" {
        "crsql_bundle_static.lib"
    } else {
        "libcrsql_bundle_static.a"
    };
    assert!(
        rust_lib_dir.join(static_lib_file).exists(),
        "expected {} at {}",
        static_lib_file,
        rust_lib_dir.display()
    );

    // --- 2. Compile the C shims bound directly to SQLite (SQLITE_CORE). ---
    let mut cc = cc::Build::new();
    cc.define("SQLITE_CORE", None)
        .define("HAVE_GETHOSTUUID", "0")
        .include(&c_src)
        .include(&sqlite_hdr)
        .file(c_src.join("crsqlite.c"))
        .file(c_src.join("changes-vtab.c"))
        .file(c_src.join("ext-data.c"))
        .warnings(false)
        .flag_if_supported("-Wno-unused-parameter");
    cc.compile("crsqlite_c");

    // --- 3. Link the Rust static bundle. ---
    println!("cargo:rustc-link-search=native={}", rust_lib_dir.display());
    println!("cargo:rustc-link-lib=static=crsql_bundle_static");

    let _ = Path::new(&sqlite_hdr); // keep sqlite_hdr referenced for clarity
}
