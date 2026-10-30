// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { DashboardSuggestions } from "../DashboardSuggestions";
import { setServerDown } from "../../lib/connectionState";
import type { SuggestionItem, SuggestionsSnapshot } from "../../lib/types";

const { fetchSuggestions, prepareSuggestion, refreshSuggestions, dismissSuggestion, wide } = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  prepareSuggestion: vi.fn(),
  refreshSuggestions: vi.fn(),
  dismissSuggestion: vi.fn(),
  wide: { value: true },
}));

vi.mock("../../lib/api", () => ({ fetchSuggestions, prepareSuggestion, refreshSuggestions, dismissSuggestion }));
vi.mock("../../hooks/useIsWideViewport", () => ({ useIsWideViewport: () => wide.value }));

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
const snap = (items: SuggestionItem[], status: "ok" | "pending" = "ok"): SuggestionsSnapshot => ({
  items,
  providers: [{ id: "github", name: "GitHub", status, sources: [] }],
});
const ok = { ok: true, data: undefined };

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.restoreAllMocks();
  setServerDown(false);
  wide.value = true;
});

const renderIt = (props: Partial<Parameters<typeof DashboardSuggestions>[0]> = {}) =>
  render(<DashboardSuggestions onPrepare={vi.fn()} onViewAll={vi.fn()} {...props} />);

describe("DashboardSuggestions", () => {
  it("caps rows (3 wide, 2 narrow) and counts all non-dismissed items in View all", async () => {
    fetchSuggestions.mockResolvedValue(snap([1, 2, 3, 4, 5].map((n) => mk(n)).concat(mk(6, { dismissed: true }))));
    const onViewAll = vi.fn();
    const { unmount } = renderIt({ onViewAll });
    expect(await screen.findAllByRole("button", { name: "Start" })).toHaveLength(3);
    fireEvent.click(screen.getByRole("link", { name: "View all (5)" }));
    expect(onViewAll).toHaveBeenCalled();
    expect(screen.getByRole("link", { name: "View all (5)" }).getAttribute("href")).toBe("/suggestions");
    unmount();

    wide.value = false;
    renderIt();
    expect(await screen.findAllByRole("button", { name: "Start" })).toHaveLength(2);
  });

  it("renders nothing without a provider", async () => {
    fetchSuggestions.mockResolvedValue({ items: [], providers: [] });
    const { container } = renderIt();
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalled());
    await act(async () => {});
    expect(container.innerHTML).toBe("");
  });

  it("Start prepares and opens the wizard prefill without creating anything", async () => {
    fetchSuggestions.mockResolvedValue(snap([mk(1)]));
    prepareSuggestion.mockResolvedValue({
      ok: true,
      data: {
        title: "T",
        initial_prompt: "do it",
        source_ref: "ex/1",
        project: { name: "aoe", path: "/p" },
        tool: "claude",
        yolo: false,
        sandbox: false,
        worktree: true,
      },
    });
    const onPrepare = vi.fn();
    renderIt({ onPrepare });
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    await waitFor(() => expect(onPrepare).toHaveBeenCalled());
    expect(prepareSuggestion).toHaveBeenCalledWith("github", "i1");
    expect(onPrepare.mock.calls[0][0]).toMatchObject({ path: "/p", initialPrompt: "do it", tool: "claude" });
  });

  it("Dismiss shows an undo bar that restores the item, and the bar expires after 10s", async () => {
    fetchSuggestions.mockResolvedValueOnce(snap([mk(1)])).mockResolvedValue(snap([mk(1, { dismissed: true })]));
    dismissSuggestion.mockResolvedValue(ok);
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
    expect(await screen.findByText(/Dismissed/)).toBeTruthy();
    expect(dismissSuggestion).toHaveBeenCalledWith("github", "i1", true);

    fireEvent.click(screen.getByRole("button", { name: "Undo" }));
    await waitFor(() => expect(dismissSuggestion).toHaveBeenCalledWith("github", "i1", false));
    await waitFor(() => expect(screen.queryByText(/Dismissed/)).toBeNull());
  });

  it("expires the undo bar after 10s", async () => {
    fetchSuggestions.mockResolvedValueOnce(snap([mk(1)])).mockResolvedValue(snap([mk(1, { dismissed: true })]));
    dismissSuggestion.mockResolvedValue(ok);
    const spy = vi.spyOn(globalThis, "setTimeout");
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
    await screen.findByText(/Dismissed/);
    const expire = spy.mock.calls.find(([, ms]) => ms === 10_000)?.[0] as () => void;
    act(() => expire());
    expect(screen.queryByText(/Dismissed/)).toBeNull();
  });

  it("shows the first-results note while a provider is pending and a refresh note when empty", async () => {
    fetchSuggestions.mockResolvedValue(snap([], "pending"));
    const { unmount } = renderIt();
    expect(await screen.findByText(/Getting your first suggestions from GitHub/)).toBeTruthy();
    unmount();

    fetchSuggestions.mockResolvedValue(snap([]));
    refreshSuggestions.mockResolvedValue(ok);
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(refreshSuggestions).toHaveBeenCalled());
  });

  it("read-only disables Start, drops Dismiss, and shows the note", async () => {
    fetchSuggestions.mockResolvedValue(snap([mk(1)]));
    renderIt({ readOnly: true });
    const start = await screen.findByRole("button", { name: "Start" });
    expect((start as HTMLButtonElement).disabled).toBe(true);
    expect(start.getAttribute("title")).toBe("Read-only: can't create Sessions");
    expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
    expect(screen.getByText(/This dashboard is read-only/)).toBeTruthy();
  });

  it("stays hidden when the first fetch fails, and retries on reconnect", async () => {
    setServerDown(true);
    fetchSuggestions.mockResolvedValueOnce(null).mockResolvedValue(snap([mk(1)]));
    const { container } = renderIt();
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(container.innerHTML).toBe("");

    act(() => setServerDown(false));
    expect(await screen.findByRole("button", { name: "Start" })).toBeTruthy();
  });

  it("shows the disconnected note and disables Start while offline", async () => {
    fetchSuggestions.mockResolvedValue(snap([mk(1)]));
    renderIt();
    await screen.findByRole("button", { name: "Start" });
    act(() => setServerDown(true));
    expect(screen.getByText(/Disconnected\./)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("warns when a mutation succeeds but the reload fails", async () => {
    fetchSuggestions.mockResolvedValueOnce(snap([mk(1)])).mockResolvedValue(null);
    dismissSuggestion.mockResolvedValue(ok);
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Could not load suggestions.");
  });

  it("keeps the undo bar when the restore fails", async () => {
    fetchSuggestions.mockResolvedValueOnce(snap([mk(1)])).mockResolvedValue(snap([mk(1, { dismissed: true })]));
    dismissSuggestion.mockResolvedValueOnce(ok).mockResolvedValueOnce({ ok: false, error: "boom", stale: false });
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
    fireEvent.click(await screen.findByRole("button", { name: "Undo" }));
    expect((await screen.findByRole("alert")).textContent).toBe("boom");
    expect(screen.getByRole("button", { name: "Undo" })).toBeTruthy();
  });

  it("clears a previous notice when Refresh starts", async () => {
    fetchSuggestions.mockResolvedValue(snap([]));
    refreshSuggestions
      .mockResolvedValueOnce({ ok: false, error: "refresh failed", stale: false })
      .mockResolvedValueOnce(ok);
    renderIt();
    fireEvent.click(await screen.findByRole("button", { name: "Refresh" }));
    expect((await screen.findByRole("alert")).textContent).toBe("refresh failed");
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });
});
