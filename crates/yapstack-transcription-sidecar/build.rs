//! SPIKE (AE3): builds the FluidAudio Swift shim (`swift/YapFluidAudio`) and
//! links it statically when the `fluidaudio` feature is on, on macOS only.
//! Without the feature this script does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_FLUIDAUDIO").is_none() {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    fluidaudio::build();
}

mod fluidaudio {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn build() {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let pkg = manifest.join("swift/YapFluidAudio");
        println!(
            "cargo:rerun-if-changed={}",
            pkg.join("Package.swift").display()
        );
        println!("cargo:rerun-if-changed={}", pkg.join("Sources").display());

        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "aarch64" => "arm64",
            "x86_64" => "x86_64",
            other => panic!("fluidaudio: unsupported arch {other}"),
        };
        // One Swift build dir per target dir (OUT_DIR is
        // <target>/<profile>/build/<pkg>-<hash>/out), so the package fetch and
        // FluidAudio compile are shared across profiles and rebuilds.
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let out = out_dir
            .ancestors()
            .nth(4)
            .map(|t| t.join("swift-yfa"))
            .unwrap_or_else(|| out_dir.join("swift"));
        let status = Command::new("swift")
            .args(["build", "-c", "release", "--arch", arch, "--package-path"])
            .arg(&pkg)
            .arg("--build-path")
            .arg(&out)
            .status()
            .expect("fluidaudio: failed to run `swift build`");
        assert!(status.success(), "fluidaudio: swift build failed");

        // SwiftPM's native build system and the swift-build backend (the
        // default from Swift 6.3) put the product in different places.
        let lib_dir = [
            out.join(format!("{arch}-apple-macosx")).join("release"),
            out.join("out/Products/Release"),
        ]
        .into_iter()
        .find(|d| d.join("libYapFluidAudio.a").exists())
        .expect("fluidaudio: libYapFluidAudio.a not found after swift build");
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=YapFluidAudio");

        // Swift runtime: the OS copy in /usr/lib/swift at run time; the
        // toolchain's dir at link time for the back-deploy compatibility
        // shims the Swift objects reference.
        let toolchain_swift = swift_toolchain_lib_dir();
        println!(
            "cargo:rustc-link-search=native={}",
            toolchain_swift.display()
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
