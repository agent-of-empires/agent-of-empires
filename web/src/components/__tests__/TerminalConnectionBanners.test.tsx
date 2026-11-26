// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
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
});
