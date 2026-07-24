import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { FolderDialog } from "./FolderDialog";
import { ICON_OPTIONS } from "@/lib/folder-constants";

describe("FolderDialog", () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    mode: "create" as const,
    onSubmit: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
    // Radix Popover (Floating UI) positioning needs these in jsdom.
    HTMLElement.prototype.scrollIntoView = vi.fn();
    HTMLElement.prototype.hasPointerCapture = vi.fn(() => false);
    HTMLElement.prototype.releasePointerCapture = vi.fn();
  });

  it("shows 'New Folder' title in create mode", () => {
    render(<FolderDialog {...defaultProps} mode="create" />);
    expect(screen.getByText("New Folder")).toBeInTheDocument();
  });

  it("shows 'Edit Folder' title in edit mode", () => {
    render(<FolderDialog {...defaultProps} mode="edit" />);
    expect(screen.getByText("Edit Folder")).toBeInTheDocument();
  });

  it("calls onSubmit with valid name", async () => {
    const onSubmit = vi.fn();
    render(<FolderDialog {...defaultProps} onSubmit={onSubmit} />);
    const input = screen.getByPlaceholderText("Folder name");
    await userEvent.type(input, "My Folder");
    await userEvent.click(screen.getByText("Create"));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ name: "My Folder" }),
    );
  });

  it("disables submit button when name is empty", () => {
    render(<FolderDialog {...defaultProps} />);
    const createButton = screen.getByText("Create");
    expect(createButton).toBeDisabled();
  });

  it("pre-populates fields from initialData", () => {
    render(
      <FolderDialog
        {...defaultProps}
        mode="edit"
        initialData={{ name: "Existing", description: "A description" }}
      />,
    );
    const input = screen.getByPlaceholderText("Folder name");
    expect(input).toHaveValue("Existing");
    const textarea = screen.getByPlaceholderText("Add context for AI chat...");
    expect(textarea).toHaveValue("A description");
  });

  it("shows parent context in create mode", () => {
    render(<FolderDialog {...defaultProps} parentName="Parent Folder" />);
    expect(screen.getByText("Parent Folder")).toBeInTheDocument();
    expect(screen.getByText(/Creating inside/)).toBeInTheDocument();
  });

  it("shows Save button in edit mode", () => {
    render(
      <FolderDialog
        {...defaultProps}
        mode="edit"
        initialData={{ name: "Test" }}
      />,
    );
    expect(screen.getByText("Save")).toBeInTheDocument();
  });

  describe("Context for AI (description) field", () => {
    it("renders with the AI-context label and helper text", () => {
      render(<FolderDialog {...defaultProps} />);
      expect(screen.getByText("Context for AI")).toBeInTheDocument();
      expect(
        screen.getByText(
          /Used as context when you chat with AI about sessions in this folder/i,
        ),
      ).toBeInTheDocument();
    });

    it("is editable and persists the typed value on submit", async () => {
      const onSubmit = vi.fn();
      render(<FolderDialog {...defaultProps} onSubmit={onSubmit} />);
      await userEvent.type(
        screen.getByPlaceholderText("Folder name"),
        "Work",
      );
      const textarea = screen.getByPlaceholderText(
        "Add context for AI chat...",
      );
      await userEvent.type(textarea, "Quarterly planning notes");
      expect(textarea).toHaveValue("Quarterly planning notes");
      await userEvent.click(screen.getByText("Create"));
      expect(onSubmit).toHaveBeenCalledWith(
        expect.objectContaining({ description: "Quarterly planning notes" }),
      );
    });
  });

  describe("icon picker (popover)", () => {
    /** Opens the picker and returns its floating grid once fully mounted. */
    async function openGrid() {
      await userEvent.click(screen.getByText("No icon").closest("button")!);
      return waitFor(() => {
        const content = document.querySelector(
          '[data-slot="popover-content"]',
        );
        expect(content).not.toBeNull();
        // The Ban "none" cell plus every ICON_OPTIONS entry.
        expect(content!.querySelectorAll("button").length).toBe(
          ICON_OPTIONS.length + 1,
        );
        return content as HTMLElement;
      });
    }

    it("does not mount the icon grid until the trigger is opened", () => {
      render(<FolderDialog {...defaultProps} />);
      // The compact trigger is present...
      const trigger = screen.getByText("No icon").closest("button")!;
      expect(trigger).toHaveAttribute("data-state", "closed");
      // ...but the floating grid is not in the DOM (no inline reflow).
      expect(
        document.querySelector('[data-slot="popover-content"]'),
      ).toBeNull();
    });

    it("shows the currently-selected icon label on the trigger", () => {
      render(
        <FolderDialog
          {...defaultProps}
          mode="edit"
          initialData={{ name: "Test", icon: "rocket" }}
        />,
      );
      // A real icon is selected, so the trigger reads "Change icon" (not "No icon").
      expect(screen.getByText("Change icon")).toBeInTheDocument();
      expect(screen.queryByText("No icon")).not.toBeInTheDocument();
    });

    it("opens the floating grid and closes it after a selection", async () => {
      render(
        <FolderDialog
          {...defaultProps}
          mode="edit"
          initialData={{ name: "Test" }}
        />,
      );
      const trigger = screen.getByText("No icon").closest("button")!;
      expect(trigger).toHaveAttribute("data-state", "closed");

      const grid = await openGrid();
      expect(trigger).toHaveAttribute("data-state", "open");

      // Pick the first real icon (folder). Clicking closes the popover.
      await userEvent.click(grid.querySelectorAll("button")[1] as HTMLElement);
      await waitFor(() =>
        expect(trigger).toHaveAttribute("data-state", "closed"),
      );
    });

    it("persists the chosen icon on submit", async () => {
      const onSubmit = vi.fn();
      render(
        <FolderDialog
          {...defaultProps}
          mode="edit"
          initialData={{ name: "Test" }}
          onSubmit={onSubmit}
        />,
      );
      const grid = await openGrid();
      // Index 1 is the first ICON_OPTIONS entry ("folder").
      await userEvent.click(grid.querySelectorAll("button")[1] as HTMLElement);
      await userEvent.click(screen.getByText("Save"));
      expect(onSubmit).toHaveBeenCalledWith(
        expect.objectContaining({ icon: "folder" }),
      );
    });
  });
});
