// @vitest-environment jsdom
//
// Transcript tool-card density (#1767). Pins the pieces that give the
// user back a scannable view after plan approval without touching the
// automatic grouping in AcpRuntime:
//   - useToolDensityPref persists to localStorage and defaults detailed,
//   - ToolDensityToggle reflects state via aria-pressed,
//   - compact mode collapses an otherwise-open card (a <=5 item todo),
//   - a user's per-card toggle is scoped to the active density, so
//     flipping the global toggle re-collapses an expanded card,
//   - errored cards stay auto-open even in compact mode so a failure is
//     never hidden (#1467).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen } from "@testing-library/react";
import type { ReactNode } from "react";

vi.mock("../../lib/highlighter", () => ({
  ensureThemeLoaded: vi.fn().mockResolvedValue("dark-plus"),
  getHighlighter: vi.fn().mockResolvedValue({
    codeToHtml: (code: string) => `<pre>${code}</pre>`,
  }),
  langKeyForExt: (s: string) => s,
  loadLanguage: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

import {
  ToolDensityToggle,
  ToolDisplayModeProvider,
  useToolDensityPref,
  useToolDisplayMode,
  type ToolDensity,
} from "./ToolDisplayMode";
import { ToolCard } from "./ToolCards";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { fixtures, makeError } from "./__fixtures__/toolCalls";

const STORAGE_KEY = "aoe.acp.toolDensity.v1";

function Wrap({ density, toolKey, children }: { density: ToolDensity; toolKey?: string; children: ReactNode }) {
  return (
    <AgentProfileProvider toolKey={toolKey ?? null}>
      <ToolDisplayModeProvider density={density}>{children}</ToolDisplayModeProvider>
    </AgentProfileProvider>
  );
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
});

describe("useToolDisplayMode", () => {
  it("defaults to detailed outside a provider", () => {
    const { result } = renderHook(() => useToolDisplayMode());
    expect(result.current).toBe("detailed");
  });

  it("reads the provided density inside a provider", () => {
    const { result } = renderHook(() => useToolDisplayMode(), {
      wrapper: ({ children }) => <ToolDisplayModeProvider density="compact">{children}</ToolDisplayModeProvider>,
    });
    expect(result.current).toBe("compact");
  });
});

describe("useToolDensityPref", () => {
  it("defaults detailed and toggles to compact, persisting to storage", () => {
    const { result } = renderHook(() => useToolDensityPref());
    expect(result.current[0]).toBe("detailed");
    act(() => result.current[1]());
    expect(result.current[0]).toBe("compact");
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe("compact");
    act(() => result.current[1]());
    expect(result.current[0]).toBe("detailed");
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe("detailed");
  });

  it("initialises from a previously stored compact preference", () => {
    window.localStorage.setItem(STORAGE_KEY, "compact");
    const { result } = renderHook(() => useToolDensityPref());
    expect(result.current[0]).toBe("compact");
  });
});

describe("ToolDensityToggle", () => {
  it("reflects density via aria-pressed and fires onToggle", () => {
    const onToggle = vi.fn();
    const { rerender } = render(<ToolDensityToggle density="detailed" onToggle={onToggle} />);
    const btn = screen.getByRole("button", { name: /compact tools/i });
    expect(btn.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(btn);
    expect(onToggle).toHaveBeenCalledTimes(1);
    rerender(<ToolDensityToggle density="compact" onToggle={onToggle} />);
    expect(screen.getByRole("button", { name: /compact tools/i }).getAttribute("aria-pressed")).toBe("true");
  });
});

describe("tool-card density", () => {
  it("compact collapses an otherwise-open todo card", () => {
    const { container, rerender } = render(
      <Wrap density="detailed" toolKey="claude">
        <ToolCard tool={fixtures.todoWrite} result={undefined} />
      </Wrap>,
    );
    // <=5 todos default open in detailed mode, so the list body shows.
    expect(container.textContent).toContain("Step one");

    rerender(
      <Wrap density="compact" toolKey="claude">
        <ToolCard tool={fixtures.todoWrite} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).not.toContain("Step one");
  });

  it("re-collapses a user-expanded card when density flips", () => {
    const { container, rerender } = render(
      <Wrap density="detailed" toolKey="claude">
        <ToolCard tool={fixtures.todoWrite} result={undefined} />
      </Wrap>,
    );
    // Collapse it by hand in detailed mode.
    fireEvent.click(screen.getByRole("button", { name: /items/i }));
    expect(container.textContent).not.toContain("Step one");

    // Flipping to compact must not leak the detailed-mode override; the
    // compact baseline (collapsed) applies.
    rerender(
      <Wrap density="compact" toolKey="claude">
        <ToolCard tool={fixtures.todoWrite} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).not.toContain("Step one");

    // And expanding it in compact works.
    fireEvent.click(screen.getByRole("button", { name: /items/i }));
    expect(container.textContent).toContain("Step one");
  });

  it("keeps an errored card open even in compact mode", () => {
    const { container } = render(
      <Wrap density="compact">
        <ToolCard tool={fixtures.bash} result={makeError({ toolCallId: "bash-1", text: "boom" })} />
      </Wrap>,
    );
    expect(container.textContent).toContain("boom");
  });
});
