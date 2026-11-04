//! With the `fluidaudio` feature on an Apple-silicon target, builds the
//! FluidAudio Swift shim (`swift/YapFluidAudio`), links it statically and
//! sets `cfg(fluidaudio_linked)` (ADR 0008). Anywhere else this script does
//! nothing, and a CoreML model folder is a load error at run time.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(fluidaudio_linked)");
    if std::env::var_os("CARGO_FEATURE_FLUIDAUDIO").is_none() {
        return;
    }
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os != "macos" || arch != "aarch64" {
        return;
    }
    fluidaudio::build();
    println!("cargo:rustc-cfg=fluidaudio_linked");
}

mod fluidaudio {
    use std::path::PathBuf;
    use std::process::Command;

    /// FluidAudio needs macOS 14 (the app's floor, tauri.conf.json
    /// minimumSystemVersion). Linking for an older target would mark the
    /// sidecar as running on an OS the Swift code can't.
    const DEPLOYMENT_TARGET: &str = "14.0";

    pub fn build() {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let pkg = manifest.join("swift/YapFluidAudio");
        for f in ["Package.swift", "Package.resolved", "Sources"] {
            println!("cargo:rerun-if-changed={}", pkg.join(f).display());
        }

        // One Swift build dir per target dir (OUT_DIR is
        // <target>/<profile>/build/<pkg>-<hash>/out), so the package fetch and
        // FluidAudio compile are shared across profiles and rebuilds.
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let out = out_dir
            .ancestors()
            .nth(4)
            .map(|t| t.join("swift-yfa"))
            .unwrap_or_else(|| out_dir.join("swift"));
        // `--force-resolved-versions`: build exactly what Package.resolved
        // pins, and fail rather than re-resolve when it's out of date.
        let status = Command::new("swift")
            .args([
                "build",
                "-c",
                "release",
                "--arch",
                "arm64",
                "--force-resolved-versions",
                "--package-path",
            ])
            .arg(&pkg)
            .arg("--build-path")
            .arg(&out)
            .status()
            .expect("fluidaudio: failed to run `swift build`");
        assert!(status.success(), "fluidaudio: swift build failed");

        // SwiftPM's native build system and the swift-build backend (the
        // default from Swift 6.3) put the product in different places.
        let lib_dir = [
            out.join("arm64-apple-macosx/release"),
            out.join("out/Products/Release"),
        ]
        .into_iter()
        .find(|d| d.join("libYapFluidAudio.a").exists())
        .expect("fluidaudio: libYapFluidAudio.a not found after swift build");
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=YapFluidAudio");
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET={DEPLOYMENT_TARGET}");

        // Swift runtime: the OS copy in /usr/lib/swift at run time; the
        // toolchain's dir at link time for the back-deploy compatibility
        // shims the Swift objects reference.
        println!(
            "cargo:rustc-link-search=native={}",
            swift_toolchain_lib_dir().display()
        );
        // The Swift objects' autolink entries already add `-rpath /usr/lib/swift`.
        println!("cargo:rustc-link-search=native=/usr/lib/swift");
        for fw in [
            "Foundation",
            "CoreML",
            "AVFoundation",
            "Accelerate",
            "CoreMedia",
        ] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        println!("cargo:rustc-link-lib=c++");
    }

    /// `<toolchain>/usr/lib/swift/macosx`, located from `swift` on PATH
    /// (`xcrun` needs a full Xcode for some queries; CLT-only works too).
    fn swift_toolchain_lib_dir() -> PathBuf {
        let out = Command::new("xcrun")
            .args(["--find", "swift"])
            .output()
            .expect("fluidaudio: xcrun --find swift");
        let swift = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        // <toolchain>/usr/bin/swift -> <toolchain>/usr/lib/swift/macosx
        swift
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .join("lib/swift/macosx")
    }
}
