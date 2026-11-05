import { describe, expect, it } from "vitest";
import { accelDisplayLabel } from "./accel";

describe("accelDisplayLabel", () => {
  it("names the GPU path by its backend", () => {
    expect(accelDisplayLabel("webgpu", null, false)).toBe("GPU (WebGPU)");
  });

  it("says when the GPU holds only part of the model's heavy work", () => {
    expect(accelDisplayLabel("webgpu", null, true)).toBe(
      "GPU (WebGPU, partial)",
    );
  });

  it("shows a chosen CPU load as plain CPU", () => {
    expect(accelDisplayLabel("cpu", null, false)).toBe("CPU");
  });

  it("flags a CPU load that fell back from a failed accelerator", () => {
    expect(
      accelDisplayLabel("cpu", "webgpu failed to load: no adapter", false),
    ).toBe("CPU (GPU unavailable)");
  });

  it("names a CoreML load the compute plan puts on the Neural Engine", () => {
    expect(accelDisplayLabel("ane", null, false)).toBe("Neural Engine");
  });

  it("says when the Neural Engine holds only part of the model's heavy work", () => {
    expect(accelDisplayLabel("ane", null, true)).toBe(
      "Neural Engine (partial)",
    );
  });

  it("doesn't claim the Neural Engine for a CoreML load with no compute plan", () => {
    expect(accelDisplayLabel("coreml", null, false)).toBe(
      "Neural Engine or CPU",
    );
  });

  it("falls back to the raw label for an unknown accelerator", () => {
    expect(accelDisplayLabel("cuda", null, false)).toBe("CUDA");
  });
});
