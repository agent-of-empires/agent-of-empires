// @vitest-environment jsdom
//
// Tests for the terminal-corner usage overlay: it shows the reset count once
// a summary with any recorded event arrives, and renders nothing while the
// setting is off.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import type { UsageSummary } from "../../lib/usage";
import { makeSession } from "./fixtures";

vi.mock("../../lib/api", () => ({
  fetchSessionUsage: vi.fn(),
}));

import { fetchSessionUsage } from "../../lib/api";
import { UsageOverlayEnabledContext } from "../../lib/usage";
import { UsageOverlay } from "../UsageOverlay";

const mockFetch = vi.mocked(fetchSessionUsage);

function summary(overrides: Partial<UsageSummary> = {}): UsageSummary {
  return {
    resets: 7,
    clears: 5,
    compactions: 2,
    compactionsAuto: 1,
    compactionsManual: 1,
    resumes: 0,
    prompts: 20,
    turns: 18,
    turnErrors: 0,
    contextStartedAt: "2026-09-29T11:00:00Z",
    contextPrompts: 4,
    contextTurns: 3,
    trackedSince: "2026-09-28T00:00:00Z",
    lastEventAt: "2026-09-29T11:59:00Z",
    tracked: true,
    ...overrides,
  };
}

const session = makeSession({ id: "s1", created_at: "2026-09-28T00:00:00Z" });

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("UsageOverlay", () => {
  it("shows the reset count once a summary arrives", async () => {
    mockFetch.mockResolvedValue(summary());

    render(
      <UsageOverlayEnabledContext.Provider value={true}>
        <UsageOverlay session={session} />
      </UsageOverlayEnabledContext.Provider>,
    );

    expect(await screen.findByText("7")).toBeTruthy();
    expect(screen.getByTestId("usage-overlay")).toBeTruthy();
  });

  it("renders nothing while the setting is off", () => {
    render(
      <UsageOverlayEnabledContext.Provider value={false}>
        <UsageOverlay session={session} />
      </UsageOverlayEnabledContext.Provider>,
    );

    expect(screen.queryByTestId("usage-overlay")).toBeNull();
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("renders nothing for a lifecycle-only session", async () => {
    mockFetch.mockResolvedValue(summary({ tracked: false }));

    render(
      <UsageOverlayEnabledContext.Provider value={true}>
        <UsageOverlay session={session} />
      </UsageOverlayEnabledContext.Provider>,
    );

    await vi.waitFor(() => expect(mockFetch).toHaveBeenCalled());
    expect(screen.queryByTestId("usage-overlay")).toBeNull();
  });
});
