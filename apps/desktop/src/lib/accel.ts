/**
 * User-facing name for the accelerator the sidecar reports it loaded the
 * model on. The sidecar label is a lowercase EP id (`"webgpu"`, `"cpu"`,
 * …); `fallback` is its reason string when the attempted accelerator
 * failed and the load fell back to CPU. A CPU load without a reason was
 * chosen on purpose (e.g. the model runs faster on CPU) and reads plainly.
 */
export function accelDisplayLabel(
  accel: string,
  fallback: string | null,
): string {
  switch (accel) {
    case "webgpu":
      return "GPU (WebGPU)";
    case "metal":
      return "GPU (Metal)";
    case "coreml":
      return "CoreML";
    case "cpu":
      return fallback ? "CPU (GPU unavailable)" : "CPU";
    default:
      return accel.toUpperCase();
  }
}
