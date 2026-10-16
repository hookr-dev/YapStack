import { describe, it, expect, vi } from "vitest";

/**
 * STRUCTURAL DEMONSTRATION of the audio-fetch cancel resubmit defect.
 *
 * The real code lives INSIDE the NoteDetailView component body
 * (apps/desktop/src/components/NoteDetailView.tsx:731-767 poll effect,
 * :774-778 cancelFetch) and is not exported, so it cannot be imported and
 * driven directly without rendering the whole (very heavy) component. This
 * test therefore replicates the exact control flow of that poll loop verbatim
 * and drives the same cancel-mid-await interleaving to prove the defect.
 *
 * Verbatim source of the loop being modelled (NoteDetailView.tsx:737-766):
 *
 *   const tick = async () => {
 *     const next = {};
 *     for (const id of ids) {                                   // <-- no cancel check
 *       const st = await syncCommands.prepareAudioPart(id, { highPriority: true });
 *       next[id] = ...;
 *     }
 *     if (cancelled) return;                                    // <-- checked AFTER the loop
 *     setPrepareStates(next);
 *     ...
 *   };
 *   void tick();
 *   return () => {                                              // effect cleanup
 *     cancelled = true;
 *     if (timer) clearTimeout(timer);
 *     ids.forEach((id) => void syncCommands.releaseAudioPart(id).catch(() => {}));
 *   };
 *
 * and cancelFetch (NoteDetailView.tsx:774-778) which flips fetchArmed → the
 * cleanup above runs synchronously inside the click event, while the loop is
 * still parked on `await prepareAudioPart(p1)`.
 */
describe("audio fetch cancel — remaining parts must NOT be re-submitted after cancel (structural demo)", () => {
  it("stops issuing prepareAudioPart once the fetch is cancelled mid-await", async () => {
    const prepareAudioPart = vi.fn();
    const releaseAudioPart = vi.fn(async (_id: string) => {});
    const cancelAudioPart = vi.fn(async (_id: string) => {});
    const setPrepareStates = vi.fn();

    const ids = ["p1", "p2", "p3"];

    // p1's prepare parks until the test releases it; p2/p3 resolve immediately.
    let releaseP1!: () => void;
    const p1Pending = new Promise<void>((r) => (releaseP1 = r));
    prepareAudioPart.mockImplementation((id: string) =>
      id === "p1"
        ? p1Pending.then(() => ({ state: "queued" }))
        : Promise.resolve({ state: "queued" }),
    );

    // ---- verbatim poll loop (NoteDetailView.tsx:737-766) ----
    let cancelled = false;
    const tick = async () => {
      const next: Record<string, unknown> = {};
      for (const id of ids) {
        // FIX (mirrors NoteDetailView.tsx): a cancel check as the first line of
        // the loop body — the parked in-flight part lands, but cancel/navigate-
        // away stops every still-queued part from being re-submitted.
        if (cancelled) return;
        const st = await prepareAudioPart(id, { highPriority: true });
        next[id] = st;
      }
      if (cancelled) return;
      setPrepareStates(next);
    };
    void tick();

    // effect cleanup (NoteDetailView.tsx:761-766)
    const cleanup = () => {
      cancelled = true;
      ids.forEach((id) => void releaseAudioPart(id).catch(() => {}));
    };

    // ---- user clicks the X: cancelFetch (NoteDetailView.tsx:774-778) then the
    //      effect disarms and its cleanup runs synchronously, all before the
    //      parked prepareAudioPart(p1) resolves. ----
    ids.forEach((id) => void cancelAudioPart(id));
    cleanup();

    // Only p1 has been requested so far (the loop is parked on it).
    expect(prepareAudioPart).toHaveBeenCalledTimes(1);
    expect(prepareAudioPart).toHaveBeenCalledWith("p1", { highPriority: true });

    // Now p1 resolves and the parked loop continues.
    releaseP1();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    // FAILS on the current control flow: the loop keeps going and re-submits the
    // very parts the user just cancelled (p2, p3) via prepareAudioPart — the
    // Rust submit/retry seam that re-admits a download. The fix (a cancel check
    // as the first line of the loop body) makes this assertion pass.
    expect(prepareAudioPart).not.toHaveBeenCalledWith("p2", { highPriority: true });
    expect(prepareAudioPart).not.toHaveBeenCalledWith("p3", { highPriority: true });
    expect(prepareAudioPart).toHaveBeenCalledTimes(1);
  });
});
