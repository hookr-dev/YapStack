import type { DbSegment } from "./db";

/**
 * Identifies one speaker within a session: `"mic"` / `"system"` for an
 * undiarized channel, `"<channel>:c<N>"` for diarization cluster N on that
 * channel. Cluster ids are channel-qualified because the mic and system
 * streams are diarized independently, so mic cluster 0 and system cluster 0
 * are different people.
 */
export type SpeakerKey = string;

/** Who reads the label. The model is told "You"; a person reads "Me". */
export type SpeakerAudience = "model" | "human";

/**
 * The per-segment inputs the resolver reads. `speaker_key_override` is an
 * explicit user assignment and wins over the key derived from
 * `source` + `speaker_id`.
 */
export type SpeakerSegment = Pick<DbSegment, "source" | "speaker_id"> & {
  speaker_key_override?: SpeakerKey | null;
};

/** What a session knows about one speaker, beyond the automatic defaults. */
export interface SessionSpeaker {
  display_name?: string | null;
  /** Wins over the automatic rule (the undiarized mic channel is the user). */
  is_self_override?: boolean | null;
}

export type SessionSpeakers = Readonly<Record<SpeakerKey, SessionSpeaker>>;

export function speakerKeyOf(seg: SpeakerSegment): SpeakerKey {
  if (seg.speaker_key_override) return seg.speaker_key_override;
  const channel = seg.source === "Mic" ? "mic" : "system";
  return seg.speaker_id == null ? channel : `${channel}:c${seg.speaker_id}`;
}

function clusterIndex(key: SpeakerKey): number | null {
  const m = /(?:^|:)c(\d+)$/.exec(key);
  return m ? Number(m[1]) : null;
}

/**
 * Clusters are not assumed to be the user, even on the mic: a diarized mic
 * means more than one person is in the room, and which one is the user is
 * unknown until someone says so via `is_self_override`.
 */
export function isSelfSpeaker(seg: SpeakerSegment, speakers?: SessionSpeakers): boolean {
  const key = speakerKeyOf(seg);
  return speakers?.[key]?.is_self_override ?? key === "mic";
}

/**
 * The one speaker label for a segment, used by AI context, search snippets,
 * insights, export and the transcript headers.
 *
 * Precedence: self → "You" (model) / "Me" (human); a named speaker → its
 * name; a cluster → "Speaker N" (1-indexed); otherwise "Other" / "Them".
 */
export function resolveSpeakerLabel(
  seg: SpeakerSegment,
  speakers: SessionSpeakers | undefined,
  audience: SpeakerAudience,
): string {
  if (isSelfSpeaker(seg, speakers)) return audience === "model" ? "You" : "Me";
  const key = speakerKeyOf(seg);
  const name = speakers?.[key]?.display_name?.trim();
  if (name) return name;
  const cluster = clusterIndex(key);
  if (cluster != null) return `Speaker ${cluster + 1}`;
  return audience === "model" ? "Other" : "Them";
}

/**
 * Rewrites the persisted `speakerNames` map to speaker keys. Older builds
 * stored names under the bare cluster number, shared by both channels, so a
 * bare entry is copied to both channels' keys unless a keyed entry exists.
 */
export function normalizeSpeakerNames(
  names: Readonly<Record<string, string>> | undefined,
): Record<SpeakerKey, string> {
  const out: Record<SpeakerKey, string> = {};
  if (!names) return out;
  for (const [key, name] of Object.entries(names)) {
    if (/^\d+$/.test(key)) continue;
    out[key] = name;
  }
  for (const [key, name] of Object.entries(names)) {
    if (!/^\d+$/.test(key)) continue;
    for (const channel of ["mic", "system"]) {
      out[`${channel}:c${key}`] ??= name;
    }
  }
  return out;
}

/** Builds the resolver's `speakers` input from the `speakerNames` setting. */
export function speakersFromNames(
  names: Readonly<Record<string, string>> | undefined,
): SessionSpeakers {
  const out: Record<SpeakerKey, SessionSpeaker> = {};
  for (const [key, name] of Object.entries(normalizeSpeakerNames(names))) {
    out[key] = { display_name: name };
  }
  return out;
}
