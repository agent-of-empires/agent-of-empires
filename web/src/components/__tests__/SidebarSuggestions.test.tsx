// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SidebarSuggestions } from "../sidebar/SidebarSuggestions";
import { updateSuggestionPrefs } from "../../lib/api";
import type { SuggestionItem, SuggestionsSnapshot } from "../../lib/types";

const { fetchSuggestions, startSuggestion } = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  startSuggestion: vi.fn(),
}));

vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/api")>()),
  fetchSuggestions,
  startSuggestion,
}));

const mk = (n: number, over: Partial<SuggestionItem> = {}): SuggestionItem => ({
  id: `i${n}`,
  provider_id: "github",
  title: `Item ${n}`,
  reason: "Assigned to you",
  source_url: `https://example.test/${n}`,
  source_ref: `ex/${n}`,
  preview: "",
  project: "aoe",
  criterion: "assigned",
  draft: { title: `Item ${n}`, prompt: "p" },
  dismissed: false,
  ...over,
});
const snap = (items: SuggestionItem[], sidebar: boolean): SuggestionsSnapshot => ({
  items,
  providers: [{ id: "github", name: "GitHub", status: "ok", sources: [] }],
  prefs: {
    homepage: true,
    sidebar,
    refresh_minutes: 15,
    show_source_ref: true,
    provider_order: [],
  },
});
const ITEMS = [1, 2, 3, 4].map((n) => mk(n)).concat(mk(5, { dismissed: true }));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
  localStorage.clear();
});

const renderIt = (props: Partial<Parameters<typeof SidebarSuggestions>[0]> = {}) => {
  const all = { onPrepare: vi.fn(), onViewAll: vi.fn(), onOpenSession: vi.fn(), ...props };
  render(<SidebarSuggestions {...all} />);
  return all;
};

describe("SidebarSuggestions", () => {
  it("appears once the sidebar pref is turned on elsewhere, without a remount", async () => {
    fetchSuggestions.mockResolvedValue(snap(ITEMS, false));
    renderIt();
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(screen.queryByTestId("sidebar-suggested-section")).toBeNull();

    fetchSuggestions.mockResolvedValue(snap(ITEMS, true));
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 200 })));
    await act(async () => {
      await updateSuggestionPrefs({ sidebar: true });
    });
    expect((await screen.findByTestId("sidebar-suggested-toggle")).textContent).toBe("Suggested (4)");
  });

  it("follows a refreshing provider only while the sidebar pref is on", async () => {
    vi.useFakeTimers();
    try {
      for (const [sidebar, calls] of [
        [true, 2],
        [false, 1],
      ] as const) {
        cleanup();
        fetchSuggestions.mockReset();
        const refreshing = snap(ITEMS, sidebar);
        refreshing.providers = [{ ...refreshing.providers[0]!, refreshing: true }];
        fetchSuggestions.mockResolvedValue(refreshing);
        renderIt();
        await act(async () => {});
        expect(fetchSuggestions).toHaveBeenCalledTimes(1);
        await act(async () => {
          await vi.advanceTimersByTimeAsync(2500);
        });
        expect(fetchSuggestions).toHaveBeenCalledTimes(calls);
      }
    } finally {
      vi.useRealTimers();
    }
  });

  it("starts collapsed, then lists up to 3 items and View all", async () => {
    const preparing = new Promise(() => {});
    startSuggestion.mockReturnValue(preparing);
    fetchSuggestions.mockResolvedValue(snap(ITEMS, true));
    const props = renderIt();
    const toggle = await screen.findByTestId("sidebar-suggested-toggle");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByTestId("sidebar-suggested-row")).toBeNull();

    fireEvent.click(toggle);
    const rows = screen.getAllByTestId("sidebar-suggested-row");
    expect(rows.map((r) => r.textContent)).toEqual([
      "Item 1ex/1 · Assigned to you",
      "Item 2ex/2 · Assigned to you",
      "Item 3ex/3 · Assigned to you",
    ]);
    fireEvent.click(screen.getByTestId("sidebar-suggested-view-all"));
    expect(props.onViewAll).toHaveBeenCalled();
    fireEvent.click(rows[0]!);
    expect(startSuggestion).toHaveBeenCalledWith("github", "i1", {});
  });

  it("shows the hooks review when a start needs trust, and resumes with the approval", async () => {
    startSuggestion
      .mockResolvedValueOnce({
        kind: "hooks_need_trust",
        review: { commands: { on_create: ["npm ci"] }, review_fingerprint: "fp1" },
      })
      .mockResolvedValueOnce({ kind: "started", sessionId: "s1", title: "Item 1" });
    fetchSuggestions.mockResolvedValue(snap(ITEMS, true));
    renderIt();
    fireEvent.click(await screen.findByTestId("sidebar-suggested-toggle"));
    fireEvent.click(screen.getAllByTestId("sidebar-suggested-row")[0]!);
    expect(await screen.findByText("npm ci")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Approve and start" }));
    await waitFor(() =>
      expect(startSuggestion).toHaveBeenLastCalledWith("github", "i1", {
        approve_hooks: true,
        review_fingerprint: "fp1",
      }),
    );
  });

  it("hides started suggestions, and the group is absent with nothing open", async () => {
    const started = { id: "s9", title: "Fix", status: "Running" as const, archived: false, created_at: "" };
    fetchSuggestions.mockResolvedValue(snap([mk(1, { started_session: started }), mk(2)], true));
    renderIt();
    fireEvent.click(await screen.findByTestId("sidebar-suggested-toggle"));
    expect(screen.getAllByTestId("sidebar-suggested-row").map((r) => r.textContent)).toEqual([
      "Item 2ex/2 · Assigned to you",
    ]);
    expect(screen.getByTestId("sidebar-suggested-toggle").textContent).toBe("Suggested (1)");
    cleanup();
    fetchSuggestions.mockClear();
    fetchSuggestions.mockResolvedValue(snap([mk(1, { started_session: started })], true));
    renderIt();
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(screen.queryByTestId("sidebar-suggested-section")).toBeNull();
    cleanup();

    fetchSuggestions.mockResolvedValue(snap([mk(1, { dismissed: true })], true));
    renderIt();
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(2));
    await act(async () => {});
    expect(screen.queryByTestId("sidebar-suggested-section")).toBeNull();
  });
});
