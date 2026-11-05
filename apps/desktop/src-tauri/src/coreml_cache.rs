//! CoreML's compile cache for the sidecar's Neural Engine model (ADR 0008).
//!
//! The first load of a CoreML model compiles it for the Neural Engine and
//! caches the result under
//! `~/Library/Caches/<owner>/com.apple.e5rt.e5bundlecache/<OS build>/`.
//! CoreML never reads another OS build's subdirectory again, so after a
//! macOS update the old one is dead weight.
//!
//! `<owner>` is the app's bundle identifier when the sidecar runs inside the
//! app bundle, and the sidecar's executable name when it runs unbundled
//! (`pnpm tauri dev`). Both were checked on macOS 27 (build 26A434).

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::{Path, PathBuf};

use tracing::{info, warn};

const E5_BUNDLE_CACHE: &str = "com.apple.e5rt.e5bundlecache";

/// The cache directories CoreML may compile the sidecar's model into: the
/// one keyed by `bundle_id` and the one keyed by the sidecar's file name.
pub fn sidecar_cache_dirs(caches_dir: &Path, bundle_id: &str, sidecar: &Path) -> Vec<PathBuf> {
    std::iter::once(bundle_id.as_ref())
        .chain(sidecar.file_name())
        .map(|owner: &std::ffi::OsStr| caches_dir.join(owner).join(E5_BUNDLE_CACHE))
        .collect()
}

/// Delete every subdirectory of `cache_dir` except `os_build`'s. Files and
/// symlinks are left alone, and a missing `cache_dir` is a no-op. Each
/// removal is logged; one that fails is logged and skipped.
pub fn remove_other_os_builds(cache_dir: &Path, os_build: &str) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == os_build {
            continue;
        }
        // `file_type` doesn't follow symlinks.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => info!(
                "removed CoreML cache for another OS build: {}",
                path.display()
            ),
            Err(e) => warn!("couldn't remove CoreML cache {}: {e}", path.display()),
        }
    }
}

extern "C" {
    // libSystem; `size_t` is `usize` on every Apple target.
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

/// This Mac's OS build (`kern.osversion`, e.g. `"26A434"`), the name CoreML
/// gives its per-build cache subdirectory.
pub fn current_os_build() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `buf` is writable for `len` bytes; no new value is set.
    let rc = unsafe {
        sysctlbyname(
            c"kern.osversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let build = CStr::from_bytes_until_nul(&buf[..len])
        .ok()?
        .to_str()
        .ok()?;
    (!build.is_empty()).then(|| build.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn removes_other_os_builds_and_keeps_the_current_one() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join(E5_BUNDLE_CACHE);
        mkdir(&cache.join("26A434/EF0C16E3"));
        mkdir(&cache.join("25B78/110D8BC6"));
        mkdir(&cache.join("24G90"));
        std::fs::write(cache.join("26A434/EF0C16E3/model.bin"), b"x").unwrap();

        remove_other_os_builds(&cache, "26A434");

        let mut left: Vec<String> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["26A434"]);
        assert!(cache.join("26A434/EF0C16E3/model.bin").exists());
    }

    #[test]
    fn leaves_files_symlinks_and_everything_outside_the_cache_alone() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join(E5_BUNDLE_CACHE);
        mkdir(&cache);
        std::fs::write(cache.join("index"), b"x").unwrap();
        let outside = root.path().join("outside/25B78");
        mkdir(&outside);
        std::fs::write(outside.join("keep"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, cache.join("25B78")).unwrap();

        remove_other_os_builds(&cache, "26A434");

        assert!(cache.join("index").exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn a_missing_cache_dir_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        remove_other_os_builds(&root.path().join(E5_BUNDLE_CACHE), "26A434");
    }

    #[test]
    fn the_cache_is_keyed_by_bundle_id_or_sidecar_name() {
        let caches = Path::new("/Users/u/Library/Caches");
        let sidecar =
            Path::new("/Applications/YapStack.app/Contents/MacOS/yapstack-transcription-sidecar");
        assert_eq!(
            sidecar_cache_dirs(caches, "dev.yapstack.app", sidecar),
            [
                caches.join("dev.yapstack.app").join(E5_BUNDLE_CACHE),
                caches
                    .join("yapstack-transcription-sidecar")
                    .join(E5_BUNDLE_CACHE),
            ]
        );
    }

    #[test]
    fn reads_this_macs_os_build() {
        let build = current_os_build().expect("kern.osversion");
        assert!(
            build.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "{build}"
        );
    }
}
