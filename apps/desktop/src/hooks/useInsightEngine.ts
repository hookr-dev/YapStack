import { useEffect, useMemo, useRef } from "react";
import { useAppStore } from "@/stores/appStore";
import {
  assembleTranscriptContext,
  isVisibleSegment,
  resolveAndCreateClient,
} from "@/lib/ai";
import {
  EMPTY_RESULT_PLACEHOLDER,
  PREVIOUS_CONTEXT_MAX_CHARS,
  evaluateTrigger,
  resolveTriggerConfig,
  type Insight,
} from "@/lib/insights";
import type { DbSegment } from "@/lib/db";
import { countWords } from "@/lib/utils";
import { log } from "@/lib/logger";
import { speakersFromNames } from "@/lib/speakers";

/** Output-format guidance appended to every Insight's system prompt by the
 *  engine. Keeps users' freeform insight prompts focused on *what* to extract
 *  while the engine handles *how* the result is shaped for the small overlay
 *  surface. Markdown rendering is enabled in the overlay
 *  (`.ai-chat-markdown` styles), so encouraging it improves readability.
 *  The 12-line ceiling is enforced here at the prompt layer — we
 *  intentionally do NOT pass `max_tokens` / `max_completion_tokens` to the
 *  completion call (see `testConnection` in `lib/ai.ts`: OpenAI's newer
 *  models reject the legacy `max_tokens`, and self-hosted OpenAI-compatible
 *  servers don't all recognise `max_completion_tokens` — leaving the cap
 *  off is the portable choice across providers). The overlay's
 *  `overflow-y-auto` body is the safety net if a model overshoots. */
const OUTPUT_FORMAT_GUIDANCE =
  "\n\nOutput format: respond in 12 lines or fewer. " +
  "Use compact Markdown where it helps — `**bold**` for key terms, `-` for " +
  "short bulleted lists. Avoid heading syntax (`#`); the output renders in " +
  "a small floating overlay window.";

/** How often the scheduler evaluates whether to fire. Cheap (arithmetic +
 *  getState, no LLM); the trigger config decides whether each tick actually
 *  fires. 500 ms keeps sub-second `settle` values (e.g. the 0.8 s jargon clause
 *  boundary) resolvable — the cost is one extra arithmetic-only getState per
 *  second. */
const SCHEDULER_TICK_MS = 500;

/**
 * Insight engine — drives the Current Insight on its trigger while a live
 * session is running. Writes results to the Zustand store; the overlay
 * controller reads from there.
 *
 * A fixed ~1 s scheduler tick evaluates the pure {@link evaluateTrigger}; only
 * when it says "fire" do we make an LLM call. Discipline:
 *  - Skip if no new (visible) segments since the last fire (no call on silence).
 *  - Fire on threshold+settle (accumulate ~W new words, then a P-second pause),
 *    bounded by a floor F (min interval) and ceiling M (max wait / liveness).
 *  - At most one in-flight call; ticks during a call are dropped (no queue).
 *  - On Current Insight change, reset the watermark + trigger clocks so the
 *    next tick backfills against the session-so-far transcript.
 *  - Errors surface to `liveInsightError`; after an error we retry only at the
 *    ceiling (no per-tick spin on a broken endpoint), no fallback chain (per
 *    the surface-errors design memory).
 */
export function useInsightEngine() {
  const enabled = useAppStore((s) => s.settings.insights.enabled);
  // The engine drives the Current Insight (runtime), not the Default (which
  // is persisted and only relevant at session start). Mid-session switches
  // via the overlay update `currentInsightId` directly; session-start
  // initialization copies Default → Current in the store lifecycle hooks.
  const currentInsightId = useAppStore((s) => s.currentInsightId);
  const liveTranscriptionActive = useAppStore((s) => s.liveTranscriptionActive);

  // Pull only the engine-relevant *primitives* of the current insight into the
  // dep array. Subscribing to `slots` directly would re-run the effect on every
  // prompt keystroke; the surface here (id + exists + enabled + preset) only
  // changes on real lifecycle/cadence events. The four custom knobs (W/P/F/M)
  // are intentionally NOT deps — the scheduler re-reads them fresh each tick via
  // `resolveTriggerConfig`, so Custom slider drags take effect within ~1 s
  // without tearing down and restarting the 1 s loop.
  const slots = useAppStore((s) => s.settings.insights.slots);
  const currentInsight = useMemo(
    () => slots.find((s) => s.id === currentInsightId) ?? null,
    [slots, currentInsightId],
  );
  const currentInsightExists = currentInsight !== null;
  const currentPreset = currentInsight?.trigger.preset ?? "balanced";

  const inFlightRef = useRef(false);
  const abortRef = useRef<AbortController | null>(null);
  // Ids of segments already sent to the LLM. We track ids — NOT an array index
  // — because `activeSessionSegments` is kept sorted by audio offset, so a
  // late-arriving backfill row with an earlier offset is inserted *before* the
  // current position. An index watermark would skip it (and re-send a boundary
  // row); an id set sends exactly the unsent segments regardless of order.
  const processedIdsRef = useRef<Set<string>>(new Set());
  // Trigger clocks (ms epoch). `lastFireAt` = start of the last fire (floor &
  // ceiling measure from here, start-to-start). `lastSegmentArrivedAt` = when
  // the segment count last grew (pause length = now − this). `prevSegLen`
  // detects growth/shrink of the segment array.
  const lastFireAtRef = useRef(0);
  const lastSegmentArrivedAtRef = useRef(0);
  const prevSegLenRef = useRef(0);
  // Set after a failed fire; cleared on any success. While set, threshold+settle
  // fires are suppressed so we retry only at the ceiling (M).
  const erroredRef = useRef(false);

  // Reset the watermark + trigger clocks AND clear the previous result/status
  // when the Current Insight changes. Zeroing `lastFireAt` makes the next tick
  // fire immediately via the ceiling (backfill against the session-so-far
  // transcript) without the OLD insight's content bleeding into the new
  // insight's prompt-context block or overlay render.
  //
  // Deliberately keyed on `currentInsightId` only — NOT on prompt / profile /
  // preset edits. Editing the active insight mid-session takes effect on the
  // next tick (the scheduler re-reads the slot fresh), but we do NOT reset the
  // watermark or clear the previous result: doing so on every keystroke would
  // re-send the whole session transcript, flicker the overlay to "Waiting…",
  // and reintroduce the edit-driven engine churn the primitive-only effect deps
  // were designed to avoid. The cost is one transitional tick where the prior
  // output rides along in the `<previous>` block under the new prompt — which
  // is exactly the refine-vs-pivot signal that block exists to provide. To
  // start an insight truly fresh after an edit, switch away and back.
  useEffect(() => {
    processedIdsRef.current = new Set();
    prevSegLenRef.current = 0;
    lastSegmentArrivedAtRef.current = 0;
    lastFireAtRef.current = 0;
    erroredRef.current = false;
    const s = useAppStore.getState();
    s.setLiveInsightResult(null);
    s.setLiveInsightStatus("idle");
    s.setLiveInsightError(null);
  }, [currentInsightId]);

  useEffect(() => {
    if (!enabled || !liveTranscriptionActive || !currentInsightId) {
      log.debug(
        `engine: gate closed (enabled=${enabled}, live=${liveTranscriptionActive}, currentId=${currentInsightId ?? "null"})`,
        "insights",
      );
      return;
    }
    if (!currentInsightExists) {
      log.debug("engine: current insight missing", "insights");
      return;
    }

    // Read the name fresh for the log (the effect intentionally does not
    // re-run on rename/prompt edits, so a closed-over name could be stale).
    const startName =
      useAppStore
        .getState()
        .settings.insights.slots.find((s) => s.id === currentInsightId)?.name ??
      "Insight";
    log.info(
      `engine: starting — insight="${startName}" preset=${currentPreset}`,
      "insights",
    );

    // The LLM call. Reached only when the scheduler decides to fire. Assumes
    // gating already passed; `state`/`insight`/`allSegments` are the snapshot
    // the decision was made against.
    async function fire(
      state: ReturnType<typeof useAppStore.getState>,
      insight: Insight,
      allSegments: DbSegment[],
    ) {
      let client;
      let model;
      try {
        ({ client, model } = resolveAndCreateClient(
          state.settings.aiConfig,
          insight.profileId,
        ));
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        log.warn(`fire: profile resolution failed — ${msg}`, "insights");
        // Back off to the ceiling: stamp the fire clock so the floor applies
        // and threshold fires stay suppressed until M elapses.
        lastFireAtRef.current = Date.now();
        erroredRef.current = true;
        state.setLiveInsightStatus("error");
        state.setLiveInsightError(msg);
        return;
      }

      // Only send *visible*, not-yet-sent segments (by id, order-independent).
      // The `visible` filter must match what assembleTranscriptContext emits and
      // what we mark processed below — otherwise a hidden segment included here
      // would be marked processed without being sent, so unhiding it later would
      // never surface it to the Insight. First fire (or post-switch backfill):
      // nothing processed yet, so this covers the whole session-so-far.
      const newSegments = allSegments.filter(
        (s) => !processedIdsRef.current.has(s.id) && isVisibleSegment(s),
      );
      const speakers = state.activeSessionId
        ? speakersFromNames(state.settings.speakerNames[state.activeSessionId])
        : undefined;
      const transcript = assembleTranscriptContext(newSegments, speakers);
      if (!transcript) {
        // Backstop (the scheduler already required visible new content).
        log.debug(
          `fire: skip — transcript empty after assembly (newSegments=${newSegments.length})`,
          "insights",
        );
        return;
      }

      // Thread the previous result back into the prompt so the model can
      // decide whether the topic is continuing (refine) or has shifted
      // (start over). Filter out the empty-completion placeholder — that's
      // a UI artefact, not content worth showing the model. Also skip if
      // the cached result is for a different insight (defensive).
      const prevResult = state.liveInsightResult;
      const previousContent =
        prevResult &&
        prevResult.insightId === insight.id &&
        prevResult.content !== EMPTY_RESULT_PLACEHOLDER
          ? truncatePrevious(prevResult.content)
          : null;

      const userMessage = previousContent
        ? `<previous>\n${previousContent}\n</previous>\n\n<transcript>\n${transcript}\n</transcript>`
        : transcript;

      const abort = new AbortController();
      abortRef.current = abort;
      inFlightRef.current = true;
      // Start-to-start cadence: stamp the fire clock now, before awaiting, so
      // F/M measure from fire start (a slow model can't drift the cadence).
      lastFireAtRef.current = Date.now();
      state.setLiveInsightStatus("running");
      state.setLiveInsightError(null);
      log.info(
        `fire: calling LLM — model=${model} newSegments=${newSegments.length} totalSegments=${allSegments.length} transcriptChars=${transcript.length} previousChars=${previousContent?.length ?? 0}`,
        "insights",
      );

      try {
        const response = await client.chat.completions.create(
          {
            model,
            messages: [
              {
                role: "system",
                content: insight.prompt + OUTPUT_FORMAT_GUIDANCE,
              },
              { role: "user", content: userMessage },
            ],
          },
          { signal: abort.signal },
        );
        if (abort.signal.aborted) {
          log.debug("fire: aborted before response landed", "insights");
          return;
        }
        const rawContent = response.choices[0]?.message?.content ?? "";
        const content = rawContent.trim();
        log.info(
          `fire: LLM result — chars=${content.length}${content.length === 0 ? " (empty)" : ""}`,
          "insights",
        );
        // Always advance the watermark + write a result, even when the
        // model returns nothing — otherwise the overlay sits at "Waiting…"
        // when the prompt legitimately produces "no output" runs. Mark exactly
        // the segments we sent as processed (segments that arrived during the
        // await stay unprocessed and are picked up next fire). Empty completion
        // is still a success → clear the error flag (else we'd stay stuck in
        // ceiling-only backoff).
        for (const s of newSegments) processedIdsRef.current.add(s.id);
        erroredRef.current = false;
        state.setLiveInsightResult({
          insightId: insight.id,
          content: content.length > 0 ? content : EMPTY_RESULT_PLACEHOLDER,
          generatedAt: new Date().toISOString(),
          segmentCountAtRun: allSegments.length,
        });
        state.setLiveInsightStatus("idle");
      } catch (e) {
        if (abort.signal.aborted) return;
        const msg = e instanceof Error ? e.message : String(e);
        log.warn(`fire: LLM call failed — ${msg}`, "insights");
        erroredRef.current = true;
        state.setLiveInsightStatus("error");
        state.setLiveInsightError(msg);
      } finally {
        inFlightRef.current = false;
        if (abortRef.current === abort) abortRef.current = null;
      }
    }

    // Cheap evaluator. Decides whether to fire; never blocks on the LLM.
    async function schedulerTick() {
      if (inFlightRef.current) return; // one call at a time; drop the tick

      const state = useAppStore.getState();
      if (!state.liveTranscriptionActive) return;

      const live = state.settings.insights;
      const liveCurrentId = state.currentInsightId;
      if (!live.enabled || liveCurrentId !== currentInsightId) return;

      const insight = live.slots.find((s) => s.id === currentInsightId);
      if (!insight) return;

      const now = Date.now();
      const allSegments = state.activeSessionSegments;
      const segLen = allSegments.length;
      // Stamp arrival when the segment count grows (new speech). On shrink
      // (hide/delete edits) just clamp the watermark down — don't treat a
      // deletion as "new speech arrived".
      if (segLen > prevSegLenRef.current) {
        lastSegmentArrivedAtRef.current = now;
        prevSegLenRef.current = segLen;
      } else if (segLen < prevSegLenRef.current) {
        prevSegLenRef.current = segLen;
      }

      // Count only the *visible*, not-yet-sent segments — same id set the fire
      // uses, and the same visibility filter the transcript assembly uses, so
      // we never fire on content the LLM wouldn't see (which would then hit the
      // empty-transcript backstop).
      const filteredNew = allSegments.filter(
        (s) => !processedIdsRef.current.has(s.id) && isVisibleSegment(s),
      );
      const hasNewContent = filteredNew.length > 0;
      const newWords = hasNewContent
        ? countWords(filteredNew.map((s) => s.text).join(" "))
        : 0;

      const config = resolveTriggerConfig(insight);
      const decision = evaluateTrigger({
        newWords,
        sinceFireMs: now - lastFireAtRef.current,
        sincePauseMs: now - lastSegmentArrivedAtRef.current,
        hasNewContent,
        config,
      });

      if (!decision.fire) return;
      // Error backoff: after a failure, only the ceiling retries — suppress the
      // responsive threshold+settle path so we don't spin on a broken endpoint.
      if (erroredRef.current && decision.reason === "threshold-settle") return;

      log.debug(
        `tick: fire (${decision.reason}) — newWords=${newWords} sinceFire=${Math.round((now - lastFireAtRef.current) / 1000)}s`,
        "insights",
      );
      await fire(state, insight, allSegments);
    }

    // Evaluate once immediately (snappy backfill on session start / switch,
    // where lastFireAt was zeroed → ceiling fires), then on the scheduler tick.
    // On a preset-change restart, lastFireAt is recent so the floor blocks this
    // immediate evaluation — no surprise fire on a cadence edit.
    void schedulerTick();
    const intervalId = window.setInterval(schedulerTick, SCHEDULER_TICK_MS);

    return () => {
      window.clearInterval(intervalId);
      abortRef.current?.abort();
      abortRef.current = null;
      inFlightRef.current = false;
      log.debug("engine: torn down", "insights");
    };
  }, [
    enabled,
    liveTranscriptionActive,
    currentInsightId,
    currentInsightExists,
    currentPreset,
  ]);
}

/** Cap the previous-result payload, preserving the most recent characters.
 *  For short rolling summaries this is a no-op; for any future append-style
 *  insight whose previous content can grow indefinitely, we trim the head
 *  rather than the tail so the model sees the latest list state. */
function truncatePrevious(content: string): string {
  if (content.length <= PREVIOUS_CONTEXT_MAX_CHARS) return content;
  return "…" + content.slice(-(PREVIOUS_CONTEXT_MAX_CHARS - 1));
}
