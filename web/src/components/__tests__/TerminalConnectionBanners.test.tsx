// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { TerminalConnectionBanners } from "../TerminalConnectionBanners";
import { setServerDown } from "../../lib/connectionState";

const props = { connected: false, reconnecting: true, retryCount: 1, maxRetries: 7, onRetry: () => {} };

afterEach(() => {
  cleanup();
  act(() => setServerDown(false));
  vi.useRealTimers();
});

describe("TerminalConnectionBanners", () => {
  it("names a redial only once it outlasts a resume's blip, and not while the Daemon banner says it", () => {
    vi.useFakeTimers();
    render(<TerminalConnectionBanners {...props} />);
    expect(screen.queryByText("Reconnecting…")).toBeNull();
    act(() => vi.advanceTimersByTime(2000));
    expect(screen.getByText("Reconnecting…")).toBeTruthy();
    act(() => setServerDown(true));
    expect(screen.queryByText("Reconnecting…")).toBeNull();
  });

  it("spins and disables Restart until the restart settles", async () => {
    let settle!: () => void;
    const onRestart = vi.fn(() => new Promise<void>((resolve) => (settle = resolve)));
    render(<TerminalConnectionBanners {...props} reconnecting={false} paneDead onRestart={onRestart} />);
    const button = screen.getByRole("button", { name: "Restart" }) as HTMLButtonElement;
    expect(button.querySelector(".animate-spin")).toBeNull();

    fireEvent.click(button);
    expect(onRestart).toHaveBeenCalledTimes(1);
    expect(button.disabled).toBe(true);
    expect(button.getAttribute("aria-busy")).toBe("true");
    expect(button.querySelector(".animate-spin")).not.toBeNull();

    await act(async () => settle());
    expect(button.disabled).toBe(false);
    expect(button.querySelector(".animate-spin")).toBeNull();
  });
});
