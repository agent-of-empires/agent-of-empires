// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { RejectedPrompt } from "../../lib/acpTypes";
import { RejectedPromptsStrip } from "./PromptStrips";

afterEach(() => {
  cleanup();
});

const rejected: RejectedPrompt = {
  id: "rejected-5",
  text: "look at this",
  reason: "prompt_failed",
  rejectedAt: "2026-11-18T00:00:00.000Z",
  attachments: [
    { id: "att-1", kind: "image", mimeType: "image/png", name: "shot.png", size: 3, url: "/a/att-1" },
    { id: "att-2", kind: "audio", mimeType: "audio/wav", size: 4, url: "/a/att-2" },
  ],
};

describe("RejectedPromptsStrip (#5662)", () => {
  it("names the attachments and why the prompt did not run", () => {
    render(<RejectedPromptsStrip rejected={[rejected]} onRetry={() => {}} onDismiss={() => {}} disabled={false} />);
    expect(screen.getByText("shot.png, audio")).toBeTruthy();
    expect(screen.getByText("The agent rejected the prompt.")).toBeTruthy();
    expect(screen.queryByText("Agent was busy; prompt was not sent.")).toBeNull();
  });

  it("retries the whole entry once until the resend settles", async () => {
    let settle!: () => void;
    const onRetry = vi.fn(() => new Promise<void>((resolve) => (settle = resolve)));
    render(<RejectedPromptsStrip rejected={[rejected]} onRetry={onRetry} onDismiss={() => {}} disabled={false} />);
    const button = screen.getByRole("button", { name: "Retry rejected prompt" }) as HTMLButtonElement;
    fireEvent.click(button);
    fireEvent.click(button);
    expect(onRetry).toHaveBeenCalledTimes(1);
    expect(onRetry).toHaveBeenCalledWith(rejected);
    expect(button.disabled).toBe(true);
    await act(async () => settle());
    expect(button.disabled).toBe(false);
  });
});
