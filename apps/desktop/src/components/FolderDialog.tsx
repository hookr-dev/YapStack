import { useState, useEffect } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Ban, ChevronsUpDown } from "lucide-react";
import { cn } from "@/lib/utils";
import { ICON_OPTIONS, COLOR_OPTIONS } from "@/lib/folder-constants";

export interface FolderDialogData {
  name: string;
  icon: string | null;
  color: string | null;
  description: string | null;
}

interface FolderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: "create" | "edit";
  initialData?: Partial<FolderDialogData>;
  parentId?: string;
  parentName?: string;
  onSubmit: (data: FolderDialogData) => void;
}

export function FolderDialog({
  open,
  onOpenChange,
  mode,
  initialData,
  parentName,
  onSubmit,
}: FolderDialogProps) {
  const [name, setName] = useState("");
  const [icon, setIcon] = useState<string | null>(null);
  const [color, setColor] = useState<string | null>(null);
  const [description, setDescription] = useState("");
  const [iconPickerOpen, setIconPickerOpen] = useState(false);

  const initName = initialData?.name;
  const initIcon = initialData?.icon;
  const initColor = initialData?.color;
  const initDescription = initialData?.description;

  useEffect(() => {
    if (open) {
      setName(initName ?? "");
      setIcon(initIcon ?? null);
      setColor(initColor ?? null);
      setDescription(initDescription ?? "");
      setIconPickerOpen(false);
    }
  }, [open, initName, initIcon, initColor, initDescription]);

  const selectedIconOption = icon
    ? ICON_OPTIONS.find((o) => o.name === icon)
    : null;
  const SelectedIcon = selectedIconOption?.icon ?? Ban;

  const handleSubmit = () => {
    const trimmed = name.trim();
    if (!trimmed) return;
    onSubmit({
      name: trimmed,
      icon,
      color,
      description: description.trim() || null,
    });
    onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            {mode === "create" ? "New Folder" : "Edit Folder"}
          </DialogTitle>
          <DialogDescription className="sr-only">
            {mode === "create"
              ? "Create a new folder with a name, optional icon, color, and description."
              : "Edit this folder's name, icon, color, and description."}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          {/* Parent context */}
          {mode === "create" && parentName && (
            <p className="text-xs text-muted-foreground">
              Creating inside: <span className="font-medium text-foreground">{parentName}</span>
            </p>
          )}

          {/* Name */}
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">
              Name
            </label>
            <Input
              placeholder="Folder name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
              autoFocus
            />
          </div>

          {/* Icon picker. `modal` gives the popover its own focus scope
              inside the Radix Dialog. */}
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">
              Icon
            </label>
            <Popover
              open={iconPickerOpen}
              onOpenChange={setIconPickerOpen}
              modal
            >
              <PopoverTrigger asChild>
                <button
                  type="button"
                  className="flex w-full items-center justify-between rounded-md border border-input bg-transparent px-2.5 py-1.5 text-xs transition-colors hover:bg-muted focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                >
                  <span className="flex items-center gap-2">
                    <span className="flex h-6 w-6 items-center justify-center rounded-md bg-muted">
                      <SelectedIcon
                        className={cn(
                          "h-4 w-4",
                          icon === null && "text-muted-foreground",
                        )}
                      />
                    </span>
                    <span className="text-muted-foreground">
                      {icon === null ? "No icon" : "Change icon"}
                    </span>
                  </span>
                  <ChevronsUpDown className="h-3.5 w-3.5 text-muted-foreground/60" />
                </button>
              </PopoverTrigger>
              <PopoverContent
                align="start"
                className="max-h-(--radix-popover-content-available-height) w-auto overflow-y-auto p-2"
              >
                <div className="grid grid-cols-8 gap-1">
                  <button
                    type="button"
                    aria-label="No icon"
                    aria-pressed={icon === null}
                    title="No icon"
                    className={cn(
                      "flex h-8 w-8 items-center justify-center rounded-md transition-colors",
                      icon === null
                        ? "bg-accent ring-1 ring-ring"
                        : "hover:bg-muted",
                    )}
                    onClick={() => {
                      setIcon(null);
                      setIconPickerOpen(false);
                    }}
                  >
                    <Ban className="h-3.5 w-3.5 text-muted-foreground" />
                  </button>
                  {ICON_OPTIONS.map((opt) => {
                    const Icon = opt.icon;
                    const selected = icon === opt.name;
                    return (
                      <button
                        key={opt.name}
                        type="button"
                        aria-label={opt.name}
                        aria-pressed={selected}
                        title={opt.name}
                        className={cn(
                          "flex h-8 w-8 items-center justify-center rounded-md transition-colors",
                          selected
                            ? "bg-accent ring-1 ring-ring"
                            : "hover:bg-muted",
                        )}
                        onClick={() => {
                          setIcon(opt.name);
                          setIconPickerOpen(false);
                        }}
                      >
                        <Icon className="h-4 w-4" />
                      </button>
                    );
                  })}
                </div>
              </PopoverContent>
            </Popover>
          </div>

          {/* Color palette */}
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">
              Color
            </label>
            <div className="flex items-center gap-1.5">
              {COLOR_OPTIONS.map((c, i) => (
                <button
                  key={i}
                  type="button"
                  className={cn(
                    "h-5 w-5 rounded-full border transition-all",
                    c === color
                      ? "scale-125 ring-2 ring-primary ring-offset-1 ring-offset-background"
                      : "hover:scale-110",
                    c === null && "border-muted-foreground/30",
                  )}
                  style={c ? { backgroundColor: c, borderColor: c } : undefined}
                  onClick={() => setColor(c)}
                >
                  {c === null && (
                    <Ban className="h-3 w-3 text-muted-foreground mx-auto" />
                  )}
                </button>
              ))}
            </div>
          </div>

          {/* Context for AI */}
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">
              Context for AI{" "}
              <span className="text-muted-foreground/60">(optional)</span>
            </label>
            <Textarea
              placeholder="Add context for AI chat..."
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={5}
              className="min-h-[120px] resize-y text-xs"
            />
            <p className="text-[11px] text-muted-foreground/60">
              Used as context when you chat with AI about sessions in this
              folder.
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={handleSubmit} disabled={!name.trim()}>
            {mode === "create" ? "Create" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
