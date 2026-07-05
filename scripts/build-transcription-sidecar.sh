#!/usr/bin/env bash
set -euo pipefail

# Build the yapstack-transcription-sidecar binary for one or more target triples.
#
# Usage:
#   ./scripts/build-transcription-sidecar.sh                  # release build for current host
#   ./scripts/build-transcription-sidecar.sh --dev             # debug build (faster, for development)
#   ./scripts/build-transcription-sidecar.sh aarch64-apple-darwin x86_64-apple-darwin
#
# The script copies the resulting binary into
#   apps/desktop/src-tauri/binaries/yapstack-transcription-sidecar-<triple>
# which is the naming convention Tauri expects for bundled sidecars.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARIES_DIR="$PROJECT_ROOT/apps/desktop/src-tauri/binaries"

RELEASE=true

# Default: detect host triple
detect_host_triple() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64) echo "aarch64-apple-darwin" ;;
                x86_64) echo "x86_64-apple-darwin" ;;
                *) echo "unknown-apple-darwin" ;;
            esac
            ;;
        Linux)
            echo "x86_64-unknown-linux-gnu"
            ;;
        MINGW*|MSYS*|CYGWIN*)
            echo "x86_64-pc-windows-msvc"
            ;;
        *)
            echo "unknown-unknown-unknown"
            ;;
    esac
}

# Parse flags
POSITIONAL=()
for arg in "$@"; do
    case "$arg" in
        --dev) RELEASE=false ;;
        *) POSITIONAL+=("$arg") ;;
    esac
done

if [ ${#POSITIONAL[@]} -eq 0 ]; then
    TARGETS=("$(detect_host_triple)")
else
    TARGETS=("${POSITIONAL[@]}")
fi

mkdir -p "$BINARIES_DIR"

if $RELEASE; then
    PROFILE_LABEL="release"
    CARGO_FLAGS="--release"
    PROFILE_DIR="release"
else
    PROFILE_LABEL="debug (dev)"
    CARGO_FLAGS=""
    PROFILE_DIR="debug"
fi

echo "Building yapstack-transcription-sidecar with whisper + parakeet features ($PROFILE_LABEL)..."
echo "Targets: ${TARGETS[*]}"
echo ""

for target in "${TARGETS[@]}"; do
    echo "=== Building for $target ==="

    # Both engines ship in dev + release builds. Apple targets get Metal
    # for whisper-rs and CoreML for parakeet-rs (CPU fallback is automatic
    # via parakeet-rs's `error_on_failure()` chain).
    FEATURES="whisper,parakeet"
    if [[ "$target" == *"apple"* ]]; then
        FEATURES="whisper,parakeet,metal,coreml,webgpu"
    elif [[ "$target" == *"windows"* ]]; then
        # Parakeet WebGPU (Dawn -> D3D12/Vulkan). The feature adds a
        # load-time webgpu_dawn.dll import (ort-sys links Dawn dynamically);
        # the DLL-staging block below MUST ship every dist DLL next to the
        # sidecar .exe or it fails before main(). Whisper stays CPU.
        # Runtime default is still CPU: WebGPU is opt-in via
        # YAPSTACK_PARAKEET_ACCEL=webgpu until Gate C validates it.
        FEATURES="whisper,parakeet,webgpu"
    fi

    # shellcheck disable=SC2086
    cargo build \
        $CARGO_FLAGS \
        --target "$target" \
        -p yapstack-transcription-sidecar \
        --features "$FEATURES"

    # Determine source binary path
    if [[ "$target" == *"windows"* ]]; then
        src="$PROJECT_ROOT/target/$target/$PROFILE_DIR/yapstack-transcription-sidecar.exe"
        dest="$BINARIES_DIR/yapstack-transcription-sidecar-${target}.exe"
    else
        src="$PROJECT_ROOT/target/$target/$PROFILE_DIR/yapstack-transcription-sidecar"
        dest="$BINARIES_DIR/yapstack-transcription-sidecar-${target}"
    fi

    if [ ! -f "$src" ]; then
        echo "ERROR: binary not found at $src"
        exit 1
    fi

    cp "$src" "$dest"
    chmod +x "$dest"
    # Strip is also set in Cargo profile.release, but belt-and-suspenders
    if command -v strip &>/dev/null && [[ "$target" != *"windows"* ]]; then
        strip "$dest" 2>/dev/null || true
    fi
    echo "Copied to $dest"

    # Apple targets pull in libwebgpu_dawn.dylib via parakeet-rs/webgpu → ort/webgpu.
    # The sidecar's only LC_RPATH is /usr/lib/swift, so without help dyld
    # cannot resolve @rpath/libwebgpu_dawn.dylib at process launch and the
    # sidecar fails *before* main() — taking both Whisper and Parakeet down
    # with it. We add two rpaths and copy the dylib into binaries/ so the
    # same artifact works in two layouts:
    #   - bundled:   sidecar at Contents/MacOS/, dylib at Contents/Frameworks/
    #                resolved via @executable_path/../Frameworks
    #                (Tauri copies binaries/libwebgpu_dawn.dylib there via
    #                 bundle.macOS.frameworks in tauri.conf.json)
    #   - dev:       sidecar mirrored to target/debug/, dylib already lives
    #                next to it after `cargo build`
    #                resolved via @executable_path
    if [[ "$target" == *"apple"* ]] && [[ "$FEATURES" == *"webgpu"* ]]; then
        dawn_src="$PROJECT_ROOT/target/$target/$PROFILE_DIR/libwebgpu_dawn.dylib"
        dawn_dest="$BINARIES_DIR/libwebgpu_dawn.dylib"
        if [ ! -f "$dawn_src" ]; then
            echo "ERROR: libwebgpu_dawn.dylib not found at $dawn_src" \
                 "(expected because webgpu feature is enabled)"
            exit 1
        fi
        cp "$dawn_src" "$dawn_dest"
        chmod 644 "$dawn_dest"
        echo "Copied Dawn dylib to $dawn_dest"

        # install_name_tool errors if the rpath already exists; tolerate.
        for rp in "@executable_path/../Frameworks" "@executable_path"; do
            if ! otool -l "$dest" | grep -qF " path $rp "; then
                install_name_tool -add_rpath "$rp" "$dest"
                echo "Added rpath $rp to $dest"
            fi
        done
    fi

    # Windows: ort-sys links Dawn dynamically (webgpu_dawn.dll load-time
    # import) and its copy-dylibs step drops every DLL from the pyke dist
    # next to the built exe in target/. Stage them all into binaries/ so
    # Tauri bundles them into the install root next to the sidecar .exe
    # (tauri.windows.conf.json bundle.resources). No rpath dance: Windows
    # resolves DLLs from the exe's own directory first.
    if [[ "$target" == *"windows"* ]] && [[ "$FEATURES" == *"webgpu"* ]]; then
        dawn_src="$PROJECT_ROOT/target/$target/$PROFILE_DIR/webgpu_dawn.dll"
        if [ ! -f "$dawn_src" ]; then
            echo "ERROR: webgpu_dawn.dll not found at $dawn_src" \
                 "(expected because webgpu feature is enabled)"
            exit 1
        fi
        shopt -s nullglob
        for dll in "$PROJECT_ROOT/target/$target/$PROFILE_DIR"/*.dll; do
            cp "$dll" "$BINARIES_DIR/$(basename "$dll")"
            echo "Copied $(basename "$dll") to $BINARIES_DIR"
        done
        shopt -u nullglob
    fi

    # Dev fallback: `find_sidecar_path()` in apps/desktop looks for
    # `<exe_dir>/yapstack-transcription-sidecar-<triple>` first, then falls back to
    # `<exe_dir>/yapstack-transcription-sidecar`. In `pnpm tauri dev` the running exe
    # lives in target/debug/, where any plain `cargo build` (e.g. via
    # `pnpm check`, `cargo test --all`, IDE rust-analyzer) rebuilds the
    # un-suffixed `yapstack-transcription-sidecar` with **default features only** —
    # no webgpu / coreml / metal — silently clobbering the feature-rich
    # build the dev runtime needs. Mirror to the triple-suffixed name
    # instead: `find_sidecar_path` picks it up first, and cargo never
    # writes to that path, so the mirror survives workspace rebuilds.
    if ! $RELEASE && [[ "$target" != *"windows"* ]]; then
        dev_runtime_dest="$PROJECT_ROOT/target/debug/yapstack-transcription-sidecar-${target}"
        if [ -d "$PROJECT_ROOT/target/debug" ]; then
            cp "$dest" "$dev_runtime_dest"
            chmod +x "$dev_runtime_dest"
            echo "Mirrored to $dev_runtime_dest (dev runtime path)"
            # Mirror the Dawn dylib next to it so the @executable_path
            # rpath resolves in dev (cargo --target puts it under
            # target/<triple>/debug/, not target/debug/).
            if [[ "$target" == *"apple"* ]] && [[ "$FEATURES" == *"webgpu"* ]]; then
                cp "$BINARIES_DIR/libwebgpu_dawn.dylib" \
                   "$PROJECT_ROOT/target/debug/libwebgpu_dawn.dylib"
                echo "Mirrored Dawn dylib to target/debug/ (dev runtime path)"
            fi
        fi
    fi
    echo ""
done

echo "Done. Sidecar binaries are in $BINARIES_DIR"
ls -lh "$BINARIES_DIR"
