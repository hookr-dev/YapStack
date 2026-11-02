import { describe, expect, it } from "vitest";
import { accelDisplayLabel } from "./accel";

describe("accelDisplayLabel", () => {
  it("names the GPU path by its backend", () => {
    expect(accelDisplayLabel("webgpu", null, false)).toBe("GPU (WebGPU)");
    expect(accelDisplayLabel("metal", null, false)).toBe("GPU (Metal)");
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

  it("falls back to the raw label for an unknown accelerator", () => {
    expect(accelDisplayLabel("cuda", null, false)).toBe("CUDA");
  });
});
