import { useMemo, useState } from "react";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Check, ChevronDown, Zap } from "lucide-react";
import type { Connection, Profile } from "@/lib/ai";
import { cn } from "@/lib/utils";

const NONE_SENTINEL = "__none__";
const DEFAULT_SENTINEL = "__default__";

export interface ProfilePickerProps {
  profiles: Profile[];
  connections: Connection[];
  value: string | null;
  onChange: (next: string | null) => void;
  /** Show a "None" option that resolves to null. Used by dictation slots. */
  allowNone?: boolean;
  /** Label for the "None" option (default: "None — no AI cleanup"). */
  noneLabel?: string;
  /** Show a "Use default" option that resolves to null. Used by Chat composer
   * to clear a per-chat override and fall back to the global assignment. */
  defaultLabel?: string;
  /** Secondary line under the "Use default" option, e.g. naming which
   * Profiles the defaults resolve to. */
  defaultSublabel?: string;
  variant?: "pill" | "inline";
  /** Optional label shown in the picker when value is null and neither
   * allowNone nor defaultLabel applies (e.g. an assignment with no Profile). */
  unassignedLabel?: string;
}

export function ProfilePicker({
  profiles,
  connections,
  value,
  onChange,
  allowNone = false,
  noneLabel = "None — no AI cleanup",
  defaultLabel,
  defaultSublabel,
  variant = "inline",
  unassignedLabel = "No profile assigned",
}: ProfilePickerProps) {
  const [open, setOpen] = useState(false);

  const grouped = useMemo(() => {
    const byConnId = new Map<string, Connection>();
    for (const c of connections) byConnId.set(c.id, c);
    const groups = new Map<string, { connection: Connection | null; profiles: Profile[] }>();
    for (const p of profiles) {
      const conn = byConnId.get(p.connectionId) ?? null;
      const key = conn?.id ?? "__disconnected__";
      if (!groups.has(key)) {
        groups.set(key, { connection: conn, profiles: [] });
      }
      groups.get(key)!.profiles.push(p);
    }
    // Put resolved groups first (in Connection iteration order), then the
    // disconnected bucket. Map preserves insertion order so we just rebuild
    // in the desired sequence.
    const ordered: { connection: Connection | null; profiles: Profile[] }[] = [];
    for (const c of connections) {
      const g = groups.get(c.id);
      if (g) ordered.push(g);
    }
    const dis = groups.get("__disconnected__");
    if (dis) ordered.push(dis);
    return ordered;
  }, [profiles, connections]);

  const currentLabel = useMemo(() => {
    if (value === null) {
      if (defaultLabel) return defaultLabel;
      if (allowNone) return noneLabel;
      return unassignedLabel;
    }
    const p = profiles.find((x) => x.id === value);
    if (!p) return "(Profile missing)";
    return p.name;
  }, [value, profiles, defaultLabel, allowNone, noneLabel, unassignedLabel]);

  function selectValue(sentinelOrId: string) {
    if (sentinelOrId === NONE_SENTINEL || sentinelOrId === DEFAULT_SENTINEL) {
      onChange(null);
    } else {
      onChange(sentinelOrId);
    }
    setOpen(false);
  }

  const hasAnyProfiles = profiles.length > 0;
  const trigger =
    variant === "pill" ? (
      <button
        type="button"
        className="inline-flex items-center gap-1 rounded-md border border-muted-foreground/20 px-2 py-0.5 text-[9px] text-muted-foreground hover:border-foreground/40 hover:text-foreground transition-colors"
      >
        <Zap className="h-2.5 w-2.5" />
        <span className="truncate max-w-[140px]">{currentLabel}</span>
        <ChevronDown className="h-2 w-2" />
      </button>
    ) : (
      <button
        type="button"
        className={cn(
          "inline-flex h-8 w-full items-center justify-between gap-2 rounded-md border border-input bg-transparent px-3 text-xs",
          "hover:bg-accent hover:text-accent-foreground transition-colors",
          value === null && "text-muted-foreground",
        )}
      >
        <span className="truncate">{currentLabel}</span>
        <ChevronDown className="h-3 w-3 shrink-0 opacity-50" />
      </button>
    );

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>{trigger}</PopoverTrigger>
      <PopoverContent
        side="bottom"
        align="start"
        className={cn(
          "max-h-[60vh] overflow-y-auto p-1",
          variant === "pill" ? "w-56" : "w-[var(--radix-popover-trigger-width)]",
        )}
        sideOffset={4}
        collisionPadding={8}
      >
        {(defaultLabel || allowNone) && (
          <div className="mb-1">
            <PickerItem
              label={defaultLabel ?? noneLabel}
              sublabel={defaultLabel ? defaultSublabel : undefined}
              selected={value === null}
              onClick={() =>
                selectValue(defaultLabel ? DEFAULT_SENTINEL : NONE_SENTINEL)
              }
              muted
            />
          </div>
        )}

        {!hasAnyProfiles && (
          <p className="px-3 py-3 text-center text-[10px] text-muted-foreground">
            No profiles yet. Create one in Settings → AI → Profiles.
          </p>
        )}

        {grouped.map((g, i) => (
          <div key={g.connection?.id ?? "disconnected"}>
            {(i > 0 || (defaultLabel || allowNone)) && (
              <div className="my-1 border-t border-border" />
            )}
            <div className="select-none px-2 pt-1.5 pb-1 text-[9px] uppercase text-muted-foreground/50">
              {g.connection ? g.connection.name : "Disconnected"}
            </div>
            {g.profiles.map((p) => (
              <PickerItem
                key={p.id}
                label={p.name}
                sublabel={
                  g.connection
                    ? p.model
                    : "Original connection deleted — reassign or delete"
                }
                selected={value === p.id}
                onClick={() => selectValue(p.id)}
                disabled={!g.connection}
              />
            ))}
          </div>
        ))}
      </PopoverContent>
    </Popover>
  );
}

function PickerItem({
  label,
  sublabel,
  selected,
  onClick,
  disabled = false,
  muted = false,
}: {
  label: string;
  sublabel?: string;
  selected: boolean;
  onClick: () => void;
  disabled?: boolean;
  muted?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "flex w-full items-center justify-between gap-2 rounded px-2 py-1.5 text-xs text-left transition-colors",
        "hover:bg-accent hover:text-accent-foreground",
        disabled && "opacity-50 cursor-not-allowed hover:bg-transparent",
        !selected && (muted ? "text-muted-foreground" : "text-foreground"),
        selected && "font-medium text-foreground",
      )}
    >
      <div className="min-w-0 flex-1">
        <div className="truncate">{label}</div>
        {sublabel && (
          <div className="truncate text-[10px] text-muted-foreground">
            {sublabel}
          </div>
        )}
      </div>
      {selected && <Check className="h-3 w-3 shrink-0 text-primary" />}
    </button>
  );
}
