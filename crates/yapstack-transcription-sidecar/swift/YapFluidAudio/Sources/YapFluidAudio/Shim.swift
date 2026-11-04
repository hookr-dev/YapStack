// SPIKE (AE3): C ABI over FluidAudio for the Rust sidecar.
//
// Surface (all strings are UTF-8, heap-allocated with strdup; free them with
// `yfa_free_string`):
//   yfa_load(dir, encoder, units, &err) -> handle | NULL
//   yfa_transcribe(handle, samples, count, &json) -> 0 | -1 (json holds the error)
//   yfa_compute_plan(path, units, &json) -> 0 | -1
//   yfa_destroy(handle)
//
// Models load from a local directory only (`AsrModels.loadLocal`), so nothing
// is ever downloaded at runtime. Results come back as JSON so the Rust side
// needs no struct ABI.

@preconcurrency import CoreML
import FluidAudio
import Foundation

private final class Box {
    let manager: AsrManager
    init(manager: AsrManager) { self.manager = manager }
}

private struct WordOut: Encodable {
    let w: String
    let s: Double
    let e: Double
}

private struct TranscribeOut: Encodable {
    let text: String
    let confidence: Float
    let duration: Double
    let processing: Double
    let words: [WordOut]
}

private func computeUnits(_ raw: Int32) -> MLComputeUnits {
    switch raw {
    case 1: return .cpuOnly
    case 2: return .cpuAndGPU
    case 3: return .all
    default: return .cpuAndNeuralEngine
    }
}

private func precision(_ raw: String) -> ParakeetEncoderPrecision {
    ParakeetEncoderPrecision(rawValue: raw) ?? .int8
}

/// Runs an async closure to completion on the calling (non-async) thread.
private func blocking<T>(_ body: @escaping () async throws -> T) throws -> T {
    let sem = DispatchSemaphore(value: 0)
    var out: Result<T, Error>!
    Task.detached {
        do { out = .success(try await body()) } catch { out = .failure(error) }
        sem.signal()
    }
    sem.wait()
    return try out.get()
}

private func put(_ s: String, _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) {
    out?.pointee = strdup(s)
}

@_cdecl("yfa_load")
public func yfa_load(
    _ dir: UnsafePointer<CChar>?,
    _ encoder: UnsafePointer<CChar>?,
    _ units: Int32,
    _ err: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> UnsafeMutableRawPointer? {
    guard let dir else { return nil }
    let url = URL(fileURLWithPath: String(cString: dir), isDirectory: true)
    let enc = encoder.map { String(cString: $0) } ?? "int8"
    do {
        let manager = try blocking { () -> AsrManager in
            let config = AsrModels.defaultConfiguration()
            config.computeUnits = computeUnits(units)
            let models = try AsrModels.loadLocal(
                from: url, version: .v3, configuration: config,
                encoderPrecision: precision(enc))
            let m = AsrManager(config: .default)
            try await m.loadModels(models)
            return m
        }
        return Unmanaged.passRetained(Box(manager: manager)).toOpaque()
    } catch {
        put("\(error)", err)
        return nil
    }
}

@_cdecl("yfa_transcribe")
public func yfa_transcribe(
    _ handle: UnsafeMutableRawPointer?,
    _ samples: UnsafePointer<Float>?,
    _ count: Int,
    _ json: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle, let samples else { return -1 }
    let box = Unmanaged<Box>.fromOpaque(handle).takeUnretainedValue()
    let audio = Array(UnsafeBufferPointer(start: samples, count: count))
    do {
        let r = try blocking { () -> ASRResult in
            // Fresh decoder state per call: every sidecar request is an
            // independent utterance, as in the ORT engine.
            var state = try TdtDecoderState()
            return try await box.manager.transcribe(audio, decoderState: &state)
        }
        let words = buildWordTimings(from: r.tokenTimings ?? []).map {
            WordOut(w: $0.word, s: $0.startTime, e: $0.endTime)
        }
        let out = TranscribeOut(
            text: r.text, confidence: r.confidence, duration: r.duration,
            processing: r.processingTime, words: words)
        let data = try JSONEncoder().encode(out)
        put(String(decoding: data, as: UTF8.self), json)
        return 0
    } catch {
        put("\(error)", json)
        return -1
    }
}

/// Per-op preferred compute device for one compiled model, from
/// `MLComputePlan` (macOS 14.4+): evidence of where CoreML puts the work.
/// JSON: {"ops": {device: count}, "cost": {device: summed estimated weight}}.
@_cdecl("yfa_compute_plan")
public func yfa_compute_plan(
    _ path: UnsafePointer<CChar>?,
    _ units: Int32,
    _ json: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let path else { return -1 }
    guard #available(macOS 14.4, *) else {
        put("MLComputePlan needs macOS 14.4", json)
        return -1
    }
    let url = URL(fileURLWithPath: String(cString: path))
    do {
        let summary = try blocking { () -> [String: [String: Double]] in
            let config = MLModelConfiguration()
            config.computeUnits = computeUnits(units)
            let plan = try await MLComputePlan.load(contentsOf: url, configuration: config)
            var ops: [String: Double] = [:]
            var cost: [String: Double] = [:]
            func device(_ d: MLComputeDevice?) -> String {
                switch d {
                case .some(.cpu): return "cpu"
                case .some(.gpu): return "gpu"
                case .some(.neuralEngine): return "ane"
                case .none: return "none"
                @unknown default: return "other"
                }
            }
            func walk(_ block: MLModelStructure.Program.Block) {
                for op in block.operations {
                    let d = device(plan.deviceUsage(for: op)?.preferred)
                    // Constants carry no compute; count them apart so they
                    // don't inflate the CPU share.
                    let key = op.operatorName == "const" ? "const" : d
                    ops[key, default: 0] += 1
                    cost[key, default: 0] += plan.estimatedCost(of: op)?.weight ?? 0
                    for b in op.blocks { walk(b) }
                }
            }
            if case let .program(program) = plan.modelStructure {
                for (_, f) in program.functions { walk(f.block) }
            }
            return ["ops": ops, "cost": cost]
        }
        let data = try JSONEncoder().encode(summary)
        put(String(decoding: data, as: UTF8.self), json)
        return 0
    } catch {
        put("\(error)", json)
        return -1
    }
}

@_cdecl("yfa_destroy")
public func yfa_destroy(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    Unmanaged<Box>.fromOpaque(handle).release()
}

@_cdecl("yfa_free_string")
public func yfa_free_string(_ s: UnsafeMutablePointer<CChar>?) {
    free(s)
}
