/**
 * User-facing name for the accelerator the sidecar reports it loaded the
 * model on. The sidecar label is a lowercase EP id (`"webgpu"`, `"cpu"`,
 * …). `fallback` is its reason string when the attempted accelerator
 * failed and the load fell back to CPU; a CPU load without a reason was
 * chosen on purpose (e.g. the model runs faster on CPU) and reads plainly.
 * `partial` means the GPU EP leaves part of the model's heavy work on the
 * CPU (the sidecar's static placement table).
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
    case "metal":
      return gpu("Metal");
    case "coreml":
      return "CoreML";
    case "cpu":
      return fallback ? "CPU (GPU unavailable)" : "CPU";
    default:
      return accel.toUpperCase();
  }
}
