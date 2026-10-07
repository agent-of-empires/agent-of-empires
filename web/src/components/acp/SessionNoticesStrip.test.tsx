// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SessionNotice } from "../../lib/acpTypes";
import { SessionNoticesStrip } from "./PromptStrips";

afterEach(() => {
  cleanup();
});

function mk(id: string, severity: string, title: string, description?: string): SessionNotice {
  return { id, severity, title, description: description ?? null };
}

describe("SessionNoticesStrip", () => {
  it("renders nothing when every notice is dismissed", () => {
    const { container } = render(<SessionNoticesStrip notices={[]} onDismiss={() => {}} />);
    expect(container.innerHTML).toBe("");
  });

  it("shows each notice with its description and dismisses the one that was clicked", () => {
    const onDismiss = vi.fn();
    render(
      <SessionNoticesStrip
        notices={[
          mk("notice-1", "warning", "Model fallback", "Switched to Sonnet."),
          mk("notice-2", "info", "Fast mode turned off"),
        ]}
        onDismiss={onDismiss}
      />,
    );

    expect(screen.getByText("Model fallback")).toBeTruthy();
    expect(screen.getByText("Switched to Sonnet.")).toBeTruthy();
    expect(screen.getByText("Fast mode turned off")).toBeTruthy();

    fireEvent.click(screen.getByLabelText("Dismiss notice: Fast mode turned off"));
    expect(onDismiss).toHaveBeenCalledTimes(1);
    expect(onDismiss).toHaveBeenCalledWith("notice-2");
  });

  // An unknown future ACP level must still render, toned as advisory.
  it("renders an unrecognized severity rather than dropping it", () => {
    render(<SessionNoticesStrip notices={[mk("notice-1", "critical", "Quota exhausted")]} onDismiss={() => {}} />);
    expect(screen.getByText("Quota exhausted")).toBeTruthy();
  });
});
