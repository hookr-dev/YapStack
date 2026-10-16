import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { toast } from "sonner";
import { AudioPlayer } from "./AudioPlayer";

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn(), info: vi.fn(), warning: vi.fn() },
}));

beforeEach(() => {
  vi.clearAllMocks();
  // jsdom doesn't implement play/pause
  HTMLAudioElement.prototype.play = vi.fn().mockResolvedValue(undefined);
  HTMLAudioElement.prototype.pause = vi.fn();
});

const ONE_PART = [{ src: "test.wav", duration: 60 }];
const TWO_PARTS = [
  { src: "p0.wav", duration: 30 },
  { src: "p1.wav", duration: 90 },
];

describe("AudioPlayer", () => {
  it("renders play button and time displays from a single part", () => {
    render(<AudioPlayer parts={[{ src: "test.wav", duration: 120 }]} />);
    expect(screen.getByText("2:00")).toBeInTheDocument();
    expect(screen.getByText("0:00")).toBeInTheDocument();
    expect(screen.getByText("1x")).toBeInTheDocument();
  });

  it("toggles to pause icon on play", async () => {
    render(<AudioPlayer parts={ONE_PART} />);
    const buttons = screen.getAllByRole("button");
    const playBtn = buttons[0];
    await userEvent.click(playBtn);
    expect(HTMLAudioElement.prototype.play).toHaveBeenCalled();
  });

  it("surfaces a factual toast when playback fails (missing/unreadable file)", async () => {
    HTMLAudioElement.prototype.play = vi
      .fn()
      .mockRejectedValue(new DOMException("no supported source", "NotSupportedError"));
    render(<AudioPlayer parts={ONE_PART} />);
    const playBtn = screen.getAllByRole("button")[0];
    await userEvent.click(playBtn);
    expect(toast.error).toHaveBeenCalledWith(
      "Couldn't play audio — file missing or unreadable",
    );
  });

  it("cycles through playback speeds", async () => {
    render(<AudioPlayer parts={ONE_PART} />);
    const speedBtn = screen.getByText("1x");
    await userEvent.click(speedBtn);
    expect(screen.getByText("1.25x")).toBeInTheDocument();
    await userEvent.click(screen.getByText("1.25x"));
    expect(screen.getByText("1.5x")).toBeInTheDocument();
  });

  it("sums durations across parts for the global timeline", () => {
    // 30 + 90 = 120 → 2:00
    render(<AudioPlayer parts={TWO_PARTS} />);
    expect(screen.getByText("2:00")).toBeInTheDocument();
  });

  it("calls onPlayStateChange when toggling play", async () => {
    const onPlayStateChange = vi.fn();
    render(
      <AudioPlayer parts={ONE_PART} onPlayStateChange={onPlayStateChange} />,
    );
    const buttons = screen.getAllByRole("button");
    await userEvent.click(buttons[0]);
    expect(onPlayStateChange).toHaveBeenCalledWith(true);
  });

  it("renders the Resume button only when onResume is provided", () => {
    const { rerender } = render(<AudioPlayer parts={ONE_PART} />);
    // No Resume → first button is play
    expect(
      screen.queryByRole("button", { name: /resume/i }),
    ).not.toBeInTheDocument();

    rerender(<AudioPlayer parts={ONE_PART} onResume={() => {}} />);
    expect(
      screen.getByRole("button", { name: /resume/i }),
    ).toBeInTheDocument();
  });
});
