// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsView } from "../SuggestionsView";
import { setServerDown } from "../../lib/connectionState";
import type { SuggestionPrepareResponse } from "../../lib/types";

const fetchSuggestions = vi.fn();
const prepareSuggestion = vi.fn();
const actOnSuggestion = vi.fn();
const refreshSuggestions = vi.fn();
const updateSuggestionPolicy = vi.fn();

vi.mock("../../lib/api", () => ({
  fetchSuggestions: (...args: unknown[]) => fetchSuggestions(...args),
  prepareSuggestion: (...args: unknown[]) => prepareSuggestion(...args),
  actOnSuggestion: (...args: unknown[]) => actOnSuggestion(...args),
  refreshSuggestions: (...args: unknown[]) => refreshSuggestions(...args),
  updateSuggestionPolicy: (...args: unknown[]) => updateSuggestionPolicy(...args),
}));

const item = {
  id: "issue-4562",
  title: "Fix session lifecycle",
  reason: "A source-backed task",
  source_url: "https://example.test/issues/4562",
  source_ref: "https://example.test/issues/4562#comment-2",
  preview: "Add the shared flow",
  project: "aoe",
  criterion: "assigned-to-me",
  draft: { title: "Fix session lifecycle", prompt: "Review issue and implement the requested flow." },
  provider_id: "github",
  provider_name: "GitHub",
  revision: 7,
  dismissed: false,
  session_id: null,
  project_path: "/work/aoe",
  can_start: true,
  start_blocked_reason: null,
};
const snapshot = {
  items: [item],
  providers: [
    { provider_id: "github", provider_name: "GitHub", status: "ok", error: null, updated_at: "2026-10-18T12:00:00Z" },
  ],
  auto_create: { enabled: false, rules: [], last_error: null },
};

afterEach(() => {
  cleanup();
  setServerDown(false);
  vi.clearAllMocks();
});

describe("SuggestionsView", () => {
  it("reviews untrusted drafts, preserves failed actions, and prepares exact identity and source reference", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    actOnSuggestion.mockResolvedValue({ ok: false, error: "Revision is stale" });
    prepareSuggestion.mockResolvedValue({
      ok: true,
      data: {
        project_path: "/work/aoe",
        title: "Fix session lifecycle",
        prompt: "Review issue and implement the requested flow.",
        source_ref: item.source_ref,
        suggestion: { provider_id: "github", id: "issue-4562", revision: 7 },
      },
    });
    const onPrepare = vi.fn();
    const onOpenSession = vi.fn();
    render(<SuggestionsView profile="work" onPrepare={onPrepare} onOpenSession={onOpenSession} />);

    expect(await screen.findByText("Fix session lifecycle")).toBeTruthy();
    expect(screen.getByText("Review issue and implement the requested flow.")).toBeTruthy();
    expect(screen.getByRole("link", { name: /View source/ }).getAttribute("href")).toBe(item.source_url);

    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() =>
      expect(actOnSuggestion).toHaveBeenCalledWith(
        { provider_id: "github", id: "issue-4562", revision: 7 },
        "dismiss",
        "work",
        false,
      ),
    );
    expect((await screen.findByRole("alert")).textContent).toContain("Revision is stale");
    expect(screen.getByText("Fix session lifecycle")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Customize" }));
    await waitFor(() =>
      expect(prepareSuggestion).toHaveBeenCalledWith({ provider_id: "github", id: "issue-4562", revision: 7 }, "work"),
    );
    expect(onPrepare).toHaveBeenCalledWith(
      expect.objectContaining({
        path: "/work/aoe",
        title: "Fix session lifecycle",
        prompt: "Review issue and implement the requested flow.",
        suggestion: { provider_id: "github", id: "issue-4562", revision: 7 },
        sourceRefs: [item.source_ref],
      }),
    );
    expect(onOpenSession).not.toHaveBeenCalled();
  });

  it("keeps automation off until an explicit provider and criterion are reviewed", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    updateSuggestionPolicy.mockResolvedValue(true);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    const enabled = await screen.findByRole("checkbox", { name: "Enable eligible rules" });
    expect((enabled as HTMLInputElement).checked).toBe(false);
    fireEvent.click(screen.getByRole("checkbox", { name: /GitHub · assigned-to-me/ }));
    await waitFor(() =>
      expect(updateSuggestionPolicy).toHaveBeenCalledWith(
        false,
        [{ provider_id: "github", criterion: "assigned-to-me" }],
        undefined,
      ),
    );
  });
  it("opens a linked session and only creates another on explicit request", async () => {
    fetchSuggestions.mockResolvedValue({
      ...snapshot,
      items: [{ ...item, session_id: "existing-session" }],
    });
    actOnSuggestion.mockResolvedValue({ ok: true, data: { session_id: "new-session", created: true } });
    const onOpenSession = vi.fn();
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={onOpenSession} />);

    fireEvent.click(await screen.findByRole("button", { name: "Open linked session" }));
    expect(onOpenSession).toHaveBeenCalledWith("existing-session");
    fireEvent.click(screen.getByRole("button", { name: "Create another" }));
    await waitFor(() =>
      expect(actOnSuggestion).toHaveBeenCalledWith(
        { provider_id: "github", id: "issue-4562", revision: 7 },
        "start",
        undefined,
        true,
      ),
    );
    expect(onOpenSession).toHaveBeenCalledWith("new-session");
  });

  it("restores a dismissed item from the persistent dismissed view", async () => {
    fetchSuggestions.mockResolvedValue({
      ...snapshot,
      items: [{ ...item, dismissed: true }],
    });
    actOnSuggestion.mockResolvedValue({ ok: true });
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    fireEvent.click(await screen.findByRole("button", { name: "Dismissed (1)" }));
    fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    await waitFor(() =>
      expect(actOnSuggestion).toHaveBeenCalledWith(
        { provider_id: "github", id: "issue-4562", revision: 7 },
        "restore",
        undefined,
        false,
      ),
    );
  });

  it("keeps the last successful snapshot visible after a failed refresh", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    refreshSuggestions.mockResolvedValue(false);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    expect(await screen.findByText("Fix session lifecycle")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect((await screen.findByRole("alert")).textContent).toContain("last successful snapshot");
    expect(screen.getByText("Fix session lifecycle")).toBeTruthy();
  });
  it("does not report a failed initial load as a true empty result", async () => {
    fetchSuggestions.mockResolvedValue(null);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.queryByText("No suggestions yet")).toBeNull();
  });
  it("clears the previous profile snapshot while the next profile is loading", async () => {
    const { promise: pendingProfile } = Promise.withResolvers<typeof snapshot>();
    fetchSuggestions.mockImplementation((profile: string) =>
      profile === "work" ? Promise.resolve(snapshot) : pendingProfile,
    );
    const onPrepare = vi.fn();
    const onOpenSession = vi.fn();
    const view = render(<SuggestionsView profile="work" onPrepare={onPrepare} onOpenSession={onOpenSession} />);

    expect(await screen.findByText("Fix session lifecycle")).toBeTruthy();
    view.rerender(<SuggestionsView profile="personal" onPrepare={onPrepare} onOpenSession={onOpenSession} />);

    expect(screen.queryByText("Fix session lifecycle")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("Loading suggestions");
  });

  it("filters projects and providers without discarding stale provider errors", async () => {
    fetchSuggestions.mockResolvedValue({
      ...snapshot,
      items: [
        item,
        { ...item, id: "linear-1", title: "Linear task", provider_id: "linear", provider_name: "Linear" },
        { ...item, id: "other", title: "Other project", project: "other" },
      ],
      providers: [{ ...snapshot.providers[0], status: "stale", error: "Rate limited" }],
    });
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);
    expect(await screen.findByText("Other project")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Project"), { target: { value: "aoe" } });
    expect(screen.queryByText("Other project")).toBeNull();
    fireEvent.change(screen.getByLabelText("Source"), { target: { value: "Linear" } });
    expect(screen.getByText("Linear task")).toBeTruthy();
    expect(screen.queryByText(item.title)).toBeNull();
    expect(screen.getByRole("region", { name: "Provider status" }).textContent).toContain("Rate limited");
    expect(screen.getByRole("region", { name: "Provider status" }).textContent).toContain(
      "Previously collected items remain available.",
    );
  });

  it.each(["read-only", "offline"])("disables mutations while %s without hiding the draft", async (mode) => {
    fetchSuggestions.mockResolvedValue(snapshot);
    if (mode === "offline") setServerDown(true);
    render(<SuggestionsView readOnly={mode === "read-only"} onPrepare={vi.fn()} onOpenSession={vi.fn()} />);
    expect(await screen.findByText(item.draft.prompt)).toBeTruthy();
    for (const name of ["Start", "Customize", "Dismiss", "Refresh"]) {
      const button = screen.getByRole("button", { name }) as HTMLButtonElement;
      expect(button.disabled).toBe(true);
      fireEvent.click(button);
    }
    expect((screen.getByRole("checkbox", { name: "Enable eligible rules" }) as HTMLInputElement).disabled).toBe(true);
    expect(actOnSuggestion).not.toHaveBeenCalled();
    expect(prepareSuggestion).not.toHaveBeenCalled();
    expect(updateSuggestionPolicy).not.toHaveBeenCalled();
  });

  it("does not open a wizard for a prepare response from a previous profile", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    const pending = Promise.withResolvers<{ ok: true; data: SuggestionPrepareResponse }>();
    prepareSuggestion.mockReturnValue(pending.promise);
    const onPrepare = vi.fn();
    const onOpenSession = vi.fn();
    const view = render(<SuggestionsView profile="work" onPrepare={onPrepare} onOpenSession={onOpenSession} />);
    fireEvent.click(await screen.findByRole("button", { name: "Customize" }));
    await waitFor(() => expect(prepareSuggestion).toHaveBeenCalled());
    view.rerender(<SuggestionsView profile="personal" onPrepare={onPrepare} onOpenSession={onOpenSession} />);
    await screen.findByText(item.title);
    await act(async () => {
      pending.resolve({
        ok: true,
        data: {
          project_path: "/work/aoe",
          title: item.title,
          prompt: item.draft.prompt,
          source_ref: item.source_ref,
          suggestion: { provider_id: item.provider_id, id: item.id, revision: item.revision },
        },
      });
      await pending.promise;
    });
    expect(onPrepare).not.toHaveBeenCalled();
  });
});
