/**
 * User-facing name for the accelerator the sidecar reports it loaded the
 * model on. The sidecar label is a lowercase id (`"webgpu"`, `"ane"`,
 * `"cpu"`, …). `fallback` is its reason string when the attempted
 * accelerator failed and the load fell back to CPU; a CPU load without a
 * reason was chosen on purpose (e.g. the model runs faster on CPU) and
 * reads plainly. `partial` means the accelerator leaves part of the model's
 * heavy work on another device (the GPU EP's static placement table, or
 * the CoreML compute plan).
 */
export function accelDisplayLabel(
  accel: string,
  fallback: string | null,
  partial: boolean,
): string {
  const gpu = (backend: string) =>
    partial ? `GPU (${backend}, partial)` : `GPU (${backend})`;
  switch (accel) {
    case "webgpu":
      return gpu("WebGPU");
    case "ane":
      return partial ? "Neural Engine + CPU" : "Neural Engine";
    // CoreML with no compute plan to say where the encoder runs.
    case "coreml":
      return "Core ML";
    case "cpu":
      return fallback ? "CPU (GPU unavailable)" : "CPU";
    default:
      return accel.toUpperCase();
  }
}
