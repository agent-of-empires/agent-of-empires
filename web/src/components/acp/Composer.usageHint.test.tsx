// @vitest-environment jsdom
//
// User stories (#2800): hovering the composer usage indicator must explain
// what the numbers mean, not just restate them. The token figure is current
// context-window usage; the dollar amount is cumulative session spend since
// the last /clear or /compact. When the agent reports no cost, the tooltip
// explains context usage and omits the cost sentence.
//
// The indicator lives in UsageHint (Composer.tsx) wrapped in the shared
// <Tooltip>, so hovering the trigger reveals the explanatory text.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";
import type { SessionUsage } from "../../lib/acpTypes";

function Harness({ usage }: { usage: SessionUsage | null }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess-usage"
        currentAgent="claude"
        availableModes={[]}
        currentModeId={null}
        legacyMode="Default"
        configOptions={[]}
        pendingConfigOption={null}
        setConfigOption={() => {}}
        sessionUsage={usage}
        availableCommands={[]}
        connected
        turnActive={false}
        queuedCount={0}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

beforeEach(() => {
  window.localStorage.clear();
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ files: [] }),
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("composer usage indicator tooltip", () => {
  it("explains context-window usage and cumulative cost on hover", () => {
    render(<Harness usage={{ used: 120_000, size: 200_000, cost: { amount: 0.42, currency: "USD" } }} />);
    // aria-label carries the same explanation for screen readers; use it to
    // locate the trigger, then hover its Tooltip wrapper (the parent span).
    const indicator = screen.getByLabelText(/Context window:/);
    fireEvent.mouseEnter(indicator.parentElement!);

    const tip = screen.getByRole("tooltip").textContent ?? "";
    expect(tip).toContain("120,000 of 200,000 tokens used (60%)");
    expect(tip).toContain("cumulative session spend since the last /clear or /compact");
  });

  it("omits the cost sentence when the agent reports no cost", () => {
    render(<Harness usage={{ used: 50_000, size: 200_000, cost: null }} />);
    const indicator = screen.getByLabelText(/Context window:/);
    fireEvent.mouseEnter(indicator.parentElement!);

    const tip = screen.getByRole("tooltip").textContent ?? "";
    expect(tip).toContain("50,000 of 200,000 tokens used (25%)");
    expect(tip).not.toContain("cumulative session spend");
  });
});
