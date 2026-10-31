import { useEffect, useState } from "react";
import { toast } from "sonner";
import { useAppStore } from "@/stores/appStore";
import { ProfilePicker } from "@/components/ai/ProfilePicker";
import {
  clearChatContextProfile,
  getChatContextProfileId,
  setChatContextProfileId,
} from "@/lib/db";

/**
 * Chat composer's per-conversation Profile picker. Reads the override row
 * from chat_context_settings; writes through setChatContextProfileId.
 * The "Default" option clears the row so the live assignments (Chat for
 * typed messages, AI actions for actions) take over again.
 *
 * Async DB read is local — there's no other consumer of the override and
 * caching it in Zustand would just be a second source of truth. State
 * resets when the contextKey changes.
 */
export function ChatProfilePickerPill({ contextKey }: { contextKey: string }) {
  const aiConfig = useAppStore((s) => s.settings.aiConfig);
  const [override, setOverride] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    if (!contextKey) {
      setOverride(null);
      return;
    }
    getChatContextProfileId(contextKey)
      .then((value) => {
        if (!cancelled) setOverride(value);
      })
      .catch(() => {
        if (!cancelled) setOverride(null);
      });
    return () => {
      cancelled = true;
    };
  }, [contextKey]);

  // Pass the raw override so "Default" is checked (and shown on the
  // trigger) whenever no override row exists. Showing the Chat default's
  // name here would be misleading: actions sent from this bar fall back to
  // the AI-actions default, not the Chat one.
  const { chatProfileId, aiActionsProfileId } = aiConfig.assignments;
  const nameOf = (id: string | null) =>
    aiConfig.profiles.find((p) => p.id === id)?.name ?? "none";
  // A dangling override (Profile deleted) is ignored by the send path, which
  // falls back to the defaults, so display it as Default too.
  const value =
    override !== null && aiConfig.profiles.some((p) => p.id === override)
      ? override
      : null;
  const defaultSublabel = `Chat: ${nameOf(chatProfileId)} · Actions: ${nameOf(aiActionsProfileId)}`;

  // Persist before updating local state: the send path reads the DB row, so
  // the pill must never show a pick the next send won't use.
  async function handleChange(next: string | null) {
    try {
      if (next === null) {
        await clearChatContextProfile(contextKey);
      } else {
        await setChatContextProfileId(contextKey, next);
      }
      setOverride(next);
    } catch (err) {
      toast.error(
        `Couldn't save the model choice: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  return (
    <ProfilePicker
      profiles={aiConfig.profiles}
      connections={aiConfig.connections}
      value={value}
      onChange={handleChange}
      defaultLabel="Default"
      defaultSublabel={defaultSublabel}
      variant="pill"
      unassignedLabel="No profile"
    />
  );
}
