/**
 * Live insights — user-defined LLM extractions that run against the live
 * session transcript and render their output in the floating Insight overlay.
 *
 * An Insight has two orthogonal axes:
 *   - **Type** — *what* it extracts (its system prompt). Templates (Rolling
 *     Summary, Glossary, Action Items, …) seed a starter prompt + a recommended
 *     cadence; editing the prompt makes the type "custom".
 *   - **Cadence** — *when* it fires (the trigger timing: Responsive / Balanced
 *     / Relaxed / Custom). Independent of content.
 *
 * Only one Insight is active at a time (the Current Insight); inactive insights
 * are dormant and consume no LLM calls.
 *
 * See docs/UBIQUITOUS_LANGUAGE.md "Live insights" section for the canonical
 * vocabulary (Insight, Insight type / template, cadence, Insight trigger,
 * Insight tick, Insight fire, Insight result, Insight overlay, Insight backfill).
 */

/** Cadence preset for an Insight's trigger — a *rhythm*, orthogonal to content.
 *  The three named tiers form a monotonic tempo scale and resolve to a fixed
 *  {@link TriggerConfig} from {@link TRIGGER_PRESETS}; `"custom"` uses the four
 *  numbers stored on the Insight's {@link InsightTrigger}. */
export type CadencePreset = "responsive" | "balanced" | "relaxed" | "custom";

/** The four levers that decide *when* an Insight fires (runs its LLM call):
 *  - threshold (W): fire after ~W new words accumulate (0 ⇒ ignore → interval).
 *  - settle (P): wait for P s of no new speech before firing (fire on the breath).
 *  - floor (F): cooldown — never fire more often than this.
 *  - ceiling (M): liveness — force a fire after M s even if the threshold is unmet. */
export interface TriggerConfig {
  thresholdWords: number; // W
  settleSeconds: number; // P
  minIntervalSeconds: number; // F (floor)
  maxWaitSeconds: number; // M (ceiling)
}

/** Persisted per-Insight trigger: a preset plus the four custom values. When
 *  `preset !== "custom"` the engine reads {@link TRIGGER_PRESETS} as the source
 *  of truth (so presets can be retuned later without a data migration); the
 *  stored numbers are the seed used when the user switches to "custom". */
export interface InsightTrigger extends TriggerConfig {
  preset: CadencePreset;
}

/** What an Insight extracts. Named types seed a starter prompt + a recommended
 *  cadence (see {@link INSIGHT_TEMPLATES}); `"custom"` is a freeform prompt the
 *  user wrote themselves. Editing a templated prompt flips the type to custom. */
export type InsightType =
  | "summary"
  | "glossary"
  | "action-items"
  | "decisions"
  | "questions"
  | "topic"
  | "custom";

export interface Insight {
  id: string; // uuid
  name: string; // user label, duplicates allowed
  type: InsightType; // what it extracts (template origin of the prompt)
  profileId: string | null; // → AI Profile id; null = unconfigured
  prompt: string; // user-editable system prompt
  trigger: InsightTrigger; // cadence — when this Insight fires
}

export interface InsightsSettings {
  enabled: boolean; // master feature toggle (persisted)
  /** What auto-loads as the Current Insight at the start of each session.
   *  Persisted. Edited only from Settings → AI → Insights. Setting this to
   *  `null` means "no auto-start" — sessions begin with no overlay; the
   *  user can pick a Current Insight via the overlay header dropdown if
   *  the overlay is already visible. */
  defaultInsightId: string | null;
  slots: Insight[];
}

export const DEFAULT_INSIGHTS_SETTINGS: InsightsSettings = {
  enabled: false,
  defaultInsightId: null,
  slots: [],
};

// Cadence values are grounded in (a) conversational speaking rate ~130 wpm
// (ASHA) ≈ 2 words/sec, mapping `thresholdWords` to a target talk-time, and
// (b) the within-speaker pause hierarchy — clause boundaries ~0.5-1 s, sentence
// ~1-1.5 s, discourse/turn ~2 s. Crucially, our transcription sidecar emits a
// segment ONLY on a Silero-VAD silence (≥500 ms) or
// a max-length cut, so a segment's arrival already marks a real pause; the
// `settle` here is *additional* trailing quiet on top of that chunker silence
// (which is why settles are short — an earlier 3-4 s settle matched a rare
// major pause, so threshold+settle almost never fired and the ceiling silently
// dominated). The three tiers form a monotonic tempo scale.
export const TRIGGER_PRESETS: Record<
  Exclude<CadencePreset, "custom">,
  TriggerConfig
> = {
  // Fast, clause-level — updates as the conversation moves.
  responsive: {
    thresholdWords: 40, // ~18 s of talk (≈2 sentences)
    settleSeconds: 0.8, // clause boundary
    minIntervalSeconds: 8,
    maxWaitSeconds: 30,
  },
  // The sensible default — a sentence/short-passage rhythm.
  balanced: {
    thresholdWords: 90, // ~40 s of talk
    settleSeconds: 1.5, // sentence/utterance boundary
    minIntervalSeconds: 15,
    maxWaitSeconds: 60,
  },
  // Patient — accrues a fuller passage and waits for a larger break.
  relaxed: {
    thresholdWords: 150, // ~70 s of talk
    settleSeconds: 2.5, // discourse/turn boundary
    minIntervalSeconds: 30,
    maxWaitSeconds: 120,
  },
};

/** Options for the cadence picker, in display order (slow→fast reads naturally
 *  top-to-bottom as responsive→relaxed; we list fastest first). */
export const CADENCE_PRESET_OPTIONS: { value: CadencePreset; label: string }[] =
  [
    { value: "responsive", label: "Responsive" },
    { value: "balanced", label: "Balanced" },
    { value: "relaxed", label: "Relaxed" },
    { value: "custom", label: "Custom" },
  ];

/** Starter prompts for each Insight type, plus the cadence each works best on.
 *  Picking a type fills the prompt and sets the cadence; the prompt stays fully
 *  editable (and editing it flips the type to "custom"). Prompts are written to
 *  exploit the engine's `<previous>` threading (refine vs. append) and assume
 *  the overlay's compact-Markdown output guidance is added by the engine. */
export const INSIGHT_TEMPLATES: Record<
  Exclude<InsightType, "custom">,
  { label: string; cadence: Exclude<CadencePreset, "custom">; prompt: string }
> = {
  summary: {
    label: "Rolling Summary",
    cadence: "relaxed",
    prompt:
      "Keep a running summary of the conversation so far — the main points and where things currently stand. Revise it as the discussion develops rather than just appending; if the topic shifts, refocus on what matters now.",
  },
  glossary: {
    label: "Glossary",
    cadence: "responsive",
    prompt:
      "List acronyms, jargon, and technical terms a newcomer might not know, each with a brief plain-language definition. Add new terms as they come up; keep ones already shown unless their meaning changes.",
  },
  "action-items": {
    label: "Action Items",
    cadence: "balanced",
    prompt:
      "Extract concrete action items and commitments — include who owns each when it's stated. List only real, actionable tasks; skip vague intentions.",
  },
  decisions: {
    label: "Decisions",
    cadence: "balanced",
    prompt:
      "Capture decisions as they're made, with a one-line rationale for each. Keep firm decisions separate from options still being weighed.",
  },
  questions: {
    label: "Open Questions",
    cadence: "balanced",
    prompt:
      "Track open questions and unresolved points raised in the discussion. Drop a question once it's clearly answered.",
  },
  topic: {
    label: "Current Topic",
    cadence: "responsive",
    prompt:
      "Name the current topic in a short phrase and add a one-line note on what's being said about it. Update promptly whenever the topic changes.",
  },
};

/** Options for the type picker, in display order (templates, then Custom). */
export const INSIGHT_TYPE_OPTIONS: { value: InsightType; label: string }[] = [
  ...(
    Object.keys(INSIGHT_TEMPLATES) as Exclude<InsightType, "custom">[]
  ).map((t) => ({ value: t as InsightType, label: INSIGHT_TEMPLATES[t].label })),
  { value: "custom", label: "Custom" },
];

/** Apply a template: the fields to merge into an Insight when its type is
 *  chosen — seeds the prompt and the recommended cadence. Pure/testable. */
export function applyTemplate(
  type: Exclude<InsightType, "custom">,
): Pick<Insight, "type" | "prompt" | "trigger"> {
  const t = INSIGHT_TEMPLATES[type];
  return {
    type,
    prompt: t.prompt,
    trigger: { preset: t.cadence, ...TRIGGER_PRESETS[t.cadence] },
  };
}

/** Clamp a single config value: non-negative finite numbers pass through;
 *  NaN / negative / non-finite fall back. Guards hand-edited Custom values and
 *  corrupted persisted data (the old `Math.max(1, …)` guard lived in the hook). */
function sanitizeConfig(config: TriggerConfig): TriggerConfig {
  const ok = (v: number, fallback: number) =>
    Number.isFinite(v) && v >= 0 ? v : fallback;
  const b = TRIGGER_PRESETS.balanced;
  return {
    thresholdWords: ok(config.thresholdWords, 0),
    settleSeconds: ok(config.settleSeconds, 0),
    minIntervalSeconds: ok(config.minIntervalSeconds, 0),
    maxWaitSeconds: ok(config.maxWaitSeconds, b.maxWaitSeconds),
  };
}

/** Resolve an Insight's effective {@link TriggerConfig}. Named presets read
 *  {@link TRIGGER_PRESETS} (source of truth); "custom" uses the stored,
 *  sanitized values. */
export function resolveTriggerConfig(insight: Insight): TriggerConfig {
  const t = insight.trigger;
  if (t.preset !== "custom") {
    return TRIGGER_PRESETS[t.preset] ?? TRIGGER_PRESETS.balanced;
  }
  return sanitizeConfig(t);
}

export type TriggerReason =
  | "no-content"
  | "floor"
  | "ceiling"
  | "threshold-settle"
  | "waiting";

export interface TriggerDecision {
  fire: boolean;
  reason: TriggerReason;
}

/** Pure decision: should the Insight fire right now? No clock, no store — the
 *  caller computes the elapsed-times and passes them in, so this is trivially
 *  unit-testable. Boundary discipline: floor uses strict `<`; settle/ceiling use
 *  `>=` — so a migrated interval tuple where F === M fires exactly on the beat
 *  rather than dead-locking. The responsive path is threshold **and** settle
 *  (accumulate enough, then fire on the next pause); the ceiling is the
 *  unconditional liveness backstop and is checked first so it wins attribution. */
export function evaluateTrigger(input: {
  newWords: number;
  sinceFireMs: number;
  sincePauseMs: number;
  hasNewContent: boolean;
  config: TriggerConfig;
}): TriggerDecision {
  const { newWords, sinceFireMs, sincePauseMs, hasNewContent, config } = input;
  if (!hasNewContent) return { fire: false, reason: "no-content" };
  if (sinceFireMs < config.minIntervalSeconds * 1000)
    return { fire: false, reason: "floor" };
  const ceilingMet = sinceFireMs >= config.maxWaitSeconds * 1000;
  const thresholdMet =
    config.thresholdWords > 0 && newWords >= config.thresholdWords;
  const settled = sincePauseMs >= config.settleSeconds * 1000;
  if (ceilingMet) return { fire: true, reason: "ceiling" };
  if (thresholdMet && settled) return { fire: true, reason: "threshold-settle" };
  return { fire: false, reason: "waiting" };
}

function formatSecs(s: number): string {
  return `${Number.isInteger(s) ? s : s.toFixed(1)}s`;
}

/** Plain-English, one-line description of an effective cadence — shown under
 *  the picker so the four numbers read as a sentence. Mirrors the semantics of
 *  {@link evaluateTrigger} exactly (threshold AND settle; ceiling as backstop). */
export function describeTrigger(config: TriggerConfig): string {
  const {
    thresholdWords: W,
    settleSeconds: P,
    minIntervalSeconds: F,
    maxWaitSeconds: M,
  } = config;

  // When F === M (and there's no accumulation trigger) the cadence is a plain
  // interval — collapse to a single "every Ns".
  if (W === 0 && F > 0 && F === M) {
    return `Updates every ${formatSecs(F)}.`;
  }
  const bounds: string[] = [];
  if (F > 0) bounds.push(`no sooner than every ${formatSecs(F)}`);
  bounds.push(`at least every ${formatSecs(M)}`);

  if (W > 0) {
    let primary = `Updates after ~${W} new words`;
    if (P > 0) primary += ` and a ${formatSecs(P)} pause`;
    return `${primary} — ${bounds.join(", ")}.`;
  }
  return `Updates ${bounds.join(", ")}.`;
}

export function buildInsight(name: string): Insight {
  return {
    id: crypto.randomUUID(),
    name: name.trim() || "New Insight",
    type: "custom",
    profileId: null,
    prompt: "",
    trigger: { preset: "balanced", ...TRIGGER_PRESETS.balanced },
  };
}

/** Whether the Insight overlay should be visible right now. The overlay must
 *  ONLY show when the feature is on, a session is live, AND the Current Insight
 *  resolves to a slot that still exists. Checking `currentInsightId !== null`
 *  alone is not enough: an Insight deleted mid-session would leave a "zombie"
 *  overlay the engine can never feed — it gates off on the same missing
 *  condition. Keep this in lockstep with the engine gate in `useInsightEngine`. */
export function shouldShowInsightOverlay(params: {
  enabled: boolean;
  liveTranscriptionActive: boolean;
  currentInsightId: string | null;
  slots: Insight[];
}): boolean {
  const { enabled, liveTranscriptionActive, currentInsightId, slots } = params;
  if (!enabled || !liveTranscriptionActive || !currentInsightId) return false;
  return slots.some((s) => s.id === currentInsightId);
}

/** Runtime state for the currently-rendering Insight result. Not persisted. */
export interface LiveInsightResult {
  insightId: string;
  content: string;
  generatedAt: string; // ISO
  segmentCountAtRun: number; // watermark for skip-if-no-new-content
}

/** Sentinel rendered when the LLM returns an empty completion for a tick.
 *  Kept as a distinct constant so the engine knows to NOT pass this back
 *  into the next prompt's `<previous>` block — it's a UI placeholder, not
 *  content the model would benefit from seeing. */
export const EMPTY_RESULT_PLACEHOLDER = "_(no output for this window)_";

/** Hard cap on how much previous content we ship in the prompt context.
 *  Most insights produce 2-3 sentences (≲ 500 chars); this is a defensive
 *  ceiling for prompts that accumulate (e.g. append-style lists). When the
 *  previous content exceeds the cap, we keep the LAST N chars — recency
 *  matters more than the oldest items. */
export const PREVIOUS_CONTEXT_MAX_CHARS = 1500;

export type LiveInsightStatus = "idle" | "running" | "error";
