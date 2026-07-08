import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  tauriCoreMock,
  tauriEventMock,
  tauriWindowMock,
  tauriDpiMock,
  tauriWebviewWindowMock,
  tauriSqlMock,
  tauriCommandsMock,
} from "@/test/tauri-mocks";

vi.mock("@tauri-apps/api/core", () => tauriCoreMock());
vi.mock("@tauri-apps/api/event", () => tauriEventMock());
vi.mock("@tauri-apps/api/window", () => tauriWindowMock());
vi.mock("@tauri-apps/api/dpi", () => tauriDpiMock());
vi.mock("@tauri-apps/api/webviewWindow", () => tauriWebviewWindowMock());
vi.mock("@/lib/db-backend", () => tauriSqlMock());
vi.mock("@/lib/tauri", () => tauriCommandsMock());
vi.mock("sonner", () => ({
  toast: { info: vi.fn(), error: vi.fn(), warning: vi.fn(), success: vi.fn() },
}));

import { useAppStore } from "./appStore";
import {
  syncCommands,
  DEFAULT_SYNC_SERVER_URL,
  type RelayProbeOk,
} from "@/lib/sync";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function okPayload(normalizedUrl: string): RelayProbeOk {
  return {
    engineVersion: "0.16.3",
    protocolVersion: 1,
    latencyMs: 42,
    normalizedUrl,
    versionAdvisory: null,
  };
}

beforeEach(() => {
  useAppStore.setState({
    relayConn: { kind: "idle" },
    syncConfig: {
      serverUrl: DEFAULT_SYNC_SERVER_URL,
      syncEnabled: false,
      email: null,
      lastChangesetSeq: 0,
      deviceFingerprint: null,
    },
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("probeRelay — success mapping + test-and-save", () => {
  it("maps ok → conn.ok and persists the normalized URL when signed out", async () => {
    vi.spyOn(syncCommands, "probe").mockResolvedValue(
      okPayload("https://self.example"),
    );

    await useAppStore.getState().probeRelay("self.example");

    const { relayConn, syncConfig } = useAppStore.getState();
    expect(relayConn).toEqual({
      kind: "ok",
      engineVersion: "0.16.3",
      protocolVersion: 1,
      latencyMs: 42,
      normalizedUrl: "https://self.example",
      versionAdvisory: null,
    });
    // Test-and-save: normalizedUrl persisted because we are signed out.
    expect(syncConfig.serverUrl).toBe("https://self.example");
  });

  it("does NOT persist the URL when signed in (locked URL)", async () => {
    useAppStore.setState((s) => ({
      syncConfig: {
        ...s.syncConfig,
        email: "a@b.com",
        serverUrl: "https://locked.example",
      },
    }));
    vi.spyOn(syncCommands, "probe").mockResolvedValue(
      okPayload("https://other.example"),
    );

    await useAppStore.getState().probeRelay("other.example");

    const { relayConn, syncConfig } = useAppStore.getState();
    expect(relayConn.kind).toBe("ok");
    // Signed-in URL is immutable via probe — unchanged.
    expect(syncConfig.serverUrl).toBe("https://locked.example");
  });

  it("sets testing synchronously before the probe resolves", () => {
    const d = deferred<RelayProbeOk>();
    vi.spyOn(syncCommands, "probe").mockReturnValue(d.promise);

    const p = useAppStore.getState().probeRelay("x.example");
    expect(useAppStore.getState().relayConn.kind).toBe("testing");
    d.resolve(okPayload("https://x.example"));
    return p;
  });
});

describe("probeRelay — reject kinds map through with verbatim raw", () => {
  it.each(["unreachable", "tls-error", "not-a-relay"] as const)(
    "%s reject → same conn kind + raw",
    async (kind) => {
      vi.spyOn(syncCommands, "probe").mockRejectedValue({
        kind,
        raw: `verbatim-${kind}`,
      });

      await useAppStore.getState().probeRelay("bad.example");

      expect(useAppStore.getState().relayConn).toEqual({
        kind,
        raw: `verbatim-${kind}`,
      });
    },
  );

  it("a non-tagged throw is surfaced verbatim as unreachable", async () => {
    vi.spyOn(syncCommands, "probe").mockRejectedValue(
      new Error("command sync_probe not found"),
    );

    await useAppStore.getState().probeRelay("bad.example");

    expect(useAppStore.getState().relayConn).toEqual({
      kind: "unreachable",
      raw: "command sync_probe not found",
    });
  });
});

describe("probeRelay — single-flight token discards stale responses", () => {
  it("discards the first (older) probe when a second resolves first", async () => {
    const dA = deferred<RelayProbeOk>();
    const dB = deferred<RelayProbeOk>();
    vi.spyOn(syncCommands, "probe")
      .mockReturnValueOnce(dA.promise)
      .mockReturnValueOnce(dB.promise);

    const store = useAppStore.getState();
    const pA = store.probeRelay("a.example"); // token 1
    const pB = store.probeRelay("b.example"); // token 2

    // B (latest) resolves first, then the stale A resolves out of order.
    dB.resolve(okPayload("https://b.example"));
    await pB;
    dA.resolve(okPayload("https://a.example"));
    await pA;

    const { relayConn, syncConfig } = useAppStore.getState();
    // Only B's result stuck; A was discarded on token mismatch.
    expect(relayConn.kind === "ok" && relayConn.normalizedUrl).toBe(
      "https://b.example",
    );
    expect(syncConfig.serverUrl).toBe("https://b.example");
  });
});

describe("resetProbe — cancel-on-edit", () => {
  it("returns conn to idle and discards an in-flight probe's late result", async () => {
    const d = deferred<RelayProbeOk>();
    vi.spyOn(syncCommands, "probe").mockReturnValue(d.promise);

    const p = useAppStore.getState().probeRelay("x.example"); // token 1
    expect(useAppStore.getState().relayConn.kind).toBe("testing");

    useAppStore.getState().resetProbe(); // bumps token → 2, idle
    expect(useAppStore.getState().relayConn.kind).toBe("idle");

    // The now-stale probe resolves — must NOT clobber idle.
    d.resolve(okPayload("https://x.example"));
    await p;
    expect(useAppStore.getState().relayConn.kind).toBe("idle");
  });
});
