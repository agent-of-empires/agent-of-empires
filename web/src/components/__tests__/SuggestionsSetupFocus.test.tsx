// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsView } from "../SuggestionsView";
import { DashboardSuggestions } from "../DashboardSuggestions";
import type { SuggestionsSnapshot } from "../../lib/types";

const { fetchSuggestions } = vi.hoisted(() => ({ fetchSuggestions: vi.fn() }));

vi.mock("../../lib/api", () => ({
  fetchAgents: () => Promise.resolve(null),
  fetchSuggestionPresets: () => Promise.resolve(null),
  fetchSuggestionProjects: () => Promise.resolve(null),
  fetchSuggestions,
}));
vi.mock("../../hooks/useIsWideViewport", () => ({ useIsWideViewport: () => true }));
// The sheet's own flow is covered in SuggestionsSetup.test.tsx; here only its completion matters.
vi.mock("../SuggestionsSetupHint", () => ({
  SuggestionsSetupHint: ({ onFinished }: { onFinished?: () => void }) => (
    <button type="button" onClick={onFinished}>
      finish-setup
    </button>
  ),
}));

const snapshot: SuggestionsSnapshot = {
  items: [],
  providers: [{ id: "github", name: "GitHub", status: "ok", enabled: true, sources: [] }],
  prefs: { homepage: true, sidebar: false, refresh_minutes: 15, show_source_ref: true, provider_order: [] },
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("focus after the setup sheet finishes", () => {
  it("the suggestions view moves focus to its heading, which outlives the removed opener", async () => {
    fetchSuggestions.mockResolvedValue({ ...snapshot, providers: [] });
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} onOpenMarketplace={vi.fn()} />);
    fireEvent.click(await screen.findByText("finish-setup"));
    expect(document.activeElement).toBe(screen.getByRole("heading", { name: "Suggested sessions" }));
  });

  it("the Dashboard section takes focus on its heading once it mounts after setup", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    render(<DashboardSuggestions focusHeading onPrepare={vi.fn()} onViewAll={vi.fn()} onOpenSession={vi.fn()} />);
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("heading", { name: "Suggested sessions" })),
    );
  });

  it("the Dashboard section leaves focus alone on an ordinary mount", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    render(<DashboardSuggestions onPrepare={vi.fn()} onViewAll={vi.fn()} onOpenSession={vi.fn()} />);
    await screen.findByRole("heading", { name: "Suggested sessions" });
    expect(document.activeElement).toBe(document.body);
  });
});
