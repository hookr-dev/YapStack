// swift-tools-version: 6.2
// SPIKE (AE3): minimal C-ABI shim over FluidAudio's Parakeet TDT v3 CoreML
// path, built by the sidecar's build.rs under the `fluidaudio` feature.
// `traits: []` builds FluidAudio ASR-only: no TTS, no diarizer and, in
// particular, no prebuilt NemoTextProcessing xcframework binary target.
import PackageDescription

let package = Package(
    name: "YapFluidAudio",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "YapFluidAudio", type: .static, targets: ["YapFluidAudio"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/FluidInference/FluidAudio.git",
            exact: "0.18.2",
            traits: []
        )
    ],
    targets: [
        .target(
            name: "YapFluidAudio",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")],
            swiftSettings: [.swiftLanguageMode(.v5)]
        )
    ]
)
