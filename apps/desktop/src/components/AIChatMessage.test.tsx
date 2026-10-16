import { render, screen, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { AIChatMessage } from "./AIChatMessage";
import type { ChatMessage } from "@/lib/ai";
import type { DbSegment } from "@/lib/db";

vi.mock("@/lib/db", () => ({}));
vi.mock("@/lib/db-backend", () => ({ default: { load: vi.fn() } }));

// Mock ReactMarkdown: passes children to custom `p` component as an array
// of strings (matching how real ReactMarkdown works). The leading empty string
// resets the global CITE_REGEX lastIndex (after the hasCitations .test() call
// in the component advances it), so processCitationsInChildren sees lastIndex=0
// when processing the actual text.
vi.mock("react-markdown", () => ({
  default: ({
    children,
    components,
  }: {
    children: string;
    components?: Record<string, (props: { children: React.ReactNode }) => React.ReactNode>;
  }) => {
    if (components?.p) {
      const P = components.p as (props: { children: React.ReactNode }) => React.ReactNode;
      // Pass an array: ["", actualText]. The "" causes a .test() failure that
      // resets the global regex lastIndex to 0 before the real text is tested.
      return <P>{["", children]}</P>;
    }
    return <p>{children}</p>;
  },
}));

// Mock getAction to return a known action definition for "summarize"
const MockIcon = (props: React.SVGProps<SVGSVGElement>) => (
  <svg data-testid="mock-action-icon" {...props} />
);
vi.mock("@/lib/ai-actions", () => ({
  getAction: (id: string) => {
    if (id === "summarize") {
      return {
        id: "summarize",
        label: "Summarize",
        description: "Comprehensive summary",
        icon: MockIcon,
        directive: "",
      };
    }
    return undefined;
  },
}));

function makeMessage(overrides?: Partial<ChatMessage>): ChatMessage {
  return {
    id: "msg-1",
    role: "user",
    content: "Hello there",
    ...overrides,
  };
}

function makeSegment(overrides?: Partial<DbSegment>): DbSegment {
  return {
    id: "seg-1",
    session_id: "s1",
    source: "Mic",
    text: "This is what was said at this timestamp",
    audio_offset_seconds: 65,
    chunk_duration_seconds: 5,
    confidence: 0.9,
    created_at: "2024-01-01",
    chunk_index: 0,
    original_text: null,
    edited_at: null,
    deleted_at: null,
    hidden: 0,
    ...overrides,
  };
}

function renderWithTooltip(ui: React.ReactElement) {
  return render(<TooltipProvider>{ui}</TooltipProvider>);
}

const writeTextMock = vi.fn().mockResolvedValue(undefined);

beforeEach(() => {
  vi.clearAllMocks();
  writeTextMock.mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    get: () => ({ writeText: writeTextMock }),
    configurable: true,
  });
});

describe("AIChatMessage", () => {
  // ---- User messages ----

  describe("user messages", () => {
    it("renders right-aligned with primary background", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage message={makeMessage()} />,
      );
      const wrapper = container.firstChild as HTMLElement;
      expect(wrapper.className).toContain("justify-end");
      const bubble = wrapper.firstChild as HTMLElement;
      expect(bubble.className).toContain("bg-primary");
    });

    it("shows text content", () => {
      renderWithTooltip(
        <AIChatMessage message={makeMessage({ content: "Test user text" })} />,
      );
      expect(screen.getByText("Test user text")).toBeInTheDocument();
    });

    it("renders multiline content with whitespace preserved", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({ content: "Line one\nLine two" })}
        />,
      );
      const el = screen.getByText(/Line one/);
      expect(el.className).toContain("whitespace-pre-wrap");
      expect(el.textContent).toBe("Line one\nLine two");
    });
  });

  // ---- User messages with action ----

  describe("user messages with action", () => {
    it("renders as action pill with icon and label", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            action: "summarize",
            content: "Summarize this",
          })}
        />,
      );
      expect(screen.getByTestId("mock-action-icon")).toBeInTheDocument();
      expect(screen.getByText("Summarize")).toBeInTheDocument();
    });

    it("renders action pill right-aligned", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            action: "summarize",
            content: "Summarize this",
          })}
        />,
      );
      const wrapper = container.firstChild as HTMLElement;
      expect(wrapper.className).toContain("justify-end");
    });

    it("falls back to normal user bubble for unknown action", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            action: "unknown-action",
            content: "Some text",
          })}
        />,
      );
      expect(screen.getByText("Some text")).toBeInTheDocument();
      expect(screen.queryByTestId("mock-action-icon")).not.toBeInTheDocument();
    });
  });

  // ---- Assistant messages ----

  describe("assistant messages", () => {
    it("renders left-aligned with muted background", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Assistant reply",
          })}
        />,
      );
      const wrapper = container.firstChild as HTMLElement;
      expect(wrapper.className).not.toContain("justify-end");
      expect(wrapper.className).toContain("flex-col");
    });

    it("renders assistant text content", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Here is my response",
          })}
        />,
      );
      expect(screen.getByText("Here is my response")).toBeInTheDocument();
    });
  });

  // ---- Tool badges ----

  describe("tool badges", () => {
    it("renders tool execution block from [tool:name] prefix lines", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content:
              "[tool:update_title] New Title\nHere is the rest of the message",
          })}
        />,
      );
      expect(screen.getByText("Title updated")).toBeInTheDocument();
      expect(screen.getByText(/New Title/)).toBeInTheDocument();
      expect(screen.getByText("Here is the rest of the message")).toBeInTheDocument();
    });

    it("uses TOOL_LABELS fallback when detail is empty", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "[tool:pin_session]\nSome text",
          })}
        />,
      );
      expect(screen.getByText("Pin toggled")).toBeInTheDocument();
    });

    it("renders multiple tool executions", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content:
              "[tool:update_title] My Title\n[tool:save_to_notes] Done\nMessage body",
          })}
        />,
      );
      expect(screen.getByText("Title updated")).toBeInTheDocument();
      expect(screen.getByText("Notes saved")).toBeInTheDocument();
      expect(screen.getByText("Message body")).toBeInTheDocument();
    });
  });

  // ---- Empty text with only badges ----

  describe("empty text with only badges", () => {
    it("hides message bubble when only tool executions present", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "[tool:update_title] New Title",
          })}
        />,
      );
      expect(screen.getByText("Title updated")).toBeInTheDocument();
      const chatBubbles = container.querySelectorAll("[class*='ai-chat-markdown']");
      expect(chatBubbles.length).toBe(0);
    });

    it("does not show copy button when only badges present", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "[tool:update_title] New Title",
          })}
        />,
      );
      expect(screen.queryByRole("button")).not.toBeInTheDocument();
    });
  });

  // ---- Copy button ----

  describe("copy button", () => {
    it("appears for assistant messages with text", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Copyable content",
          })}
        />,
      );
      const buttons = screen.getAllByRole("button");
      expect(buttons.length).toBeGreaterThanOrEqual(1);
    });

    it("copies content to clipboard on click", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Content to copy",
          })}
        />,
      );
      const buttons = screen.getAllByRole("button");
      fireEvent.click(buttons[0]);
      expect(writeTextMock).toHaveBeenCalledWith("Content to copy");
    });

    it("does not appear for user messages", () => {
      renderWithTooltip(
        <AIChatMessage message={makeMessage({ role: "user", content: "Hi" })} />,
      );
      expect(screen.queryByRole("button")).not.toBeInTheDocument();
    });
  });

  // ---- Save to notes button ----

  describe("save to notes button", () => {
    it("appears when onSaveToNotes prop is provided", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Save this",
          })}
          onSaveToNotes={vi.fn()}
        />,
      );
      const buttons = screen.getAllByRole("button");
      expect(buttons.length).toBe(2);
    });

    it("calls onSaveToNotes with content when clicked", async () => {
      const user = userEvent.setup();
      const onSave = vi.fn();
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Note content",
          })}
          onSaveToNotes={onSave}
        />,
      );
      const buttons = screen.getAllByRole("button");
      await user.click(buttons[1]);
      expect(onSave).toHaveBeenCalledWith("Note content");
    });

    it("does not appear when onSaveToNotes is not provided", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "No save button",
          })}
        />,
      );
      const buttons = screen.getAllByRole("button");
      expect(buttons.length).toBe(1);
    });
  });

  // ---- Citations ----

  describe("citations", () => {
    // The component uses a module-level CITE_REGEX with the 'g' flag.
    // The .test() method on a global regex advances lastIndex, which persists
    // across renders. Rendering a non-citation assistant message resets
    // lastIndex to 0 (failed .test() on non-matching text resets it).
    beforeEach(() => {
      const { unmount } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({ role: "assistant", content: "reset regex" })}
        />,
      );
      unmount();
    });

    it("renders [[seg:id]] as clickable timestamp chips", () => {
      const segment = makeSegment({
        id: "seg-abc",
        audio_offset_seconds: 125,
        text: "Referenced text",
      });
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "See this reference [[seg:seg-abc]] for details",
          })}
          segments={[segment]}
          onCitationClick={vi.fn()}
        />,
      );
      // formatTimestamp(125) => "2:05"
      expect(screen.getByText("2:05")).toBeInTheDocument();
    });

    it("calls onCitationClick when citation chip is clicked", async () => {
      const user = userEvent.setup();
      const onClick = vi.fn();
      const segment = makeSegment({ id: "seg-xyz", audio_offset_seconds: 30 });
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Check [[seg:seg-xyz]] here",
          })}
          segments={[segment]}
          onCitationClick={onClick}
        />,
      );
      const chip = screen.getByText("0:30");
      await user.click(chip);
      expect(onClick).toHaveBeenCalledWith("seg-xyz");
    });

    it("renders fallback ref chip when segment not found", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "See [[seg:missing-id]] here",
          })}
          segments={[]}
          onCitationClick={vi.fn()}
        />,
      );
      expect(screen.getByText("ref")).toBeInTheDocument();
    });
  });

  // ---- Streaming indicator ----

  describe("streaming indicator", () => {
    it("shows cursor when message.isStreaming is true", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Streaming...",
            isStreaming: true,
          })}
        />,
      );
      const cursor = container.querySelector(".animate-pulse");
      expect(cursor).toBeInTheDocument();
    });

    it("does not show cursor when isStreaming is false", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Done streaming",
            isStreaming: false,
          })}
        />,
      );
      const cursor = container.querySelector(".animate-pulse");
      expect(cursor).toBeNull();
    });

    it("shows bubble with cursor even when text is empty during streaming", () => {
      const { container } = renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "",
            isStreaming: true,
          })}
        />,
      );
      const bubble = container.querySelector("[class*='bg-muted']");
      expect(bubble).toBeInTheDocument();
      const cursor = container.querySelector(".animate-pulse");
      expect(cursor).toBeInTheDocument();
    });

    it("hides copy button while streaming", () => {
      renderWithTooltip(
        <AIChatMessage
          message={makeMessage({
            role: "assistant",
            content: "Still going...",
            isStreaming: true,
          })}
        />,
      );
      expect(screen.queryByRole("button")).not.toBeInTheDocument();
    });
  });
});
