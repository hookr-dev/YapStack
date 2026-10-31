import { describe, expect, it } from "vitest";
import { accelDisplayLabel } from "./accel";

describe("accelDisplayLabel", () => {
  it("names the GPU path by its backend", () => {
    expect(accelDisplayLabel("webgpu", null)).toBe("GPU (WebGPU)");
    expect(accelDisplayLabel("metal", null)).toBe("GPU (Metal)");
  });

  it("shows a chosen CPU load as plain CPU", () => {
    expect(accelDisplayLabel("cpu", null)).toBe("CPU");
  });

  it("flags a CPU load that fell back from a failed accelerator", () => {
    expect(
      accelDisplayLabel("cpu", "webgpu failed to load: no adapter"),
    ).toBe("CPU (GPU unavailable)");
  });

  it("falls back to the raw label for an unknown accelerator", () => {
    expect(accelDisplayLabel("cuda", null)).toBe("CUDA");
  });
});
