// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsView } from "../SuggestionsView";
import { setServerDown } from "../../lib/connectionState";
import type { SuggestionPrepareResponse } from "../../lib/types";

const { fetchSuggestions, prepareSuggestion, actOnSuggestion, refreshSuggestions, updateSuggestionPolicy } = vi.hoisted(
  () => ({
    fetchSuggestions: vi.fn(),
    prepareSuggestion: vi.fn(),
    actOnSuggestion: vi.fn(),
    refreshSuggestions: vi.fn(),
    updateSuggestionPolicy: vi.fn(),
  }),
);

vi.mock("../../lib/api", () => ({
  fetchSuggestions,
  prepareSuggestion,
  actOnSuggestion,
  refreshSuggestions,
  updateSuggestionPolicy,
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
  vi.resetAllMocks();
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

  it("keeps the suggestion visible when preparation fails", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    prepareSuggestion.mockResolvedValue({ ok: false, error: "Project mapping is missing" });
    const onPrepare = vi.fn();
    const onOpenSession = vi.fn();
    render(<SuggestionsView onPrepare={onPrepare} onOpenSession={onOpenSession} />);

    fireEvent.click(await screen.findByRole("button", { name: "Customize" }));

    expect((await screen.findByRole("alert")).textContent).toContain("Project mapping is missing");
    expect(screen.getByText(item.title)).toBeTruthy();
    expect(onPrepare).not.toHaveBeenCalled();
    expect(onOpenSession).not.toHaveBeenCalled();
  });

  it("starts an unlinked suggestion through the host and opens the created session", async () => {
    fetchSuggestions.mockResolvedValue(snapshot);
    actOnSuggestion.mockResolvedValue({ ok: true, data: { session_id: "new-session", created: true } });
    const onOpenSession = vi.fn();
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={onOpenSession} />);

    fireEvent.click(await screen.findByRole("button", { name: "Start" }));

    await waitFor(() =>
      expect(actOnSuggestion).toHaveBeenCalledWith(
        { provider_id: "github", id: "issue-4562", revision: 7 },
        "start",
        undefined,
        false,
      ),
    );
    expect(onOpenSession).toHaveBeenCalledWith("new-session");
  });

  it("keeps the last snapshot visible when refresh and the follow-up read both fail", async () => {
    fetchSuggestions.mockResolvedValueOnce(snapshot).mockResolvedValueOnce(null);
    refreshSuggestions.mockResolvedValue(false);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    expect(await screen.findByText(item.title)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));

    expect((await screen.findByRole("alert")).textContent).toContain("Could not load suggestions");
    expect(screen.getByText(item.title)).toBeTruthy();
    expect(screen.queryByText("No suggestions yet")).toBeNull();
  });

  it("keeps automation off until an explicit provider and criterion are reviewed", async () => {
    fetchSuggestions.mockResolvedValueOnce(snapshot).mockResolvedValue({
      ...snapshot,
      auto_create: {
        enabled: false,
        rules: [{ provider_id: "github", criterion: "assigned-to-me" }],
        last_error: null,
      },
    });
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
    await waitFor(() =>
      expect(
        (screen.getByRole("group", { name: "Reviewed providers and criteria" }) as HTMLFieldSetElement).disabled,
      ).toBe(false),
    );

    fireEvent.click(enabled);
    await waitFor(() =>
      expect(updateSuggestionPolicy).toHaveBeenLastCalledWith(
        true,
        [{ provider_id: "github", criterion: "assigned-to-me" }],
        undefined,
      ),
    );
  });

  it("serializes policy updates while preserving provider display names", async () => {
    const { promise: update, resolve: resolveUpdate } = Promise.withResolvers<boolean>();
    fetchSuggestions.mockResolvedValue({
      ...snapshot,
      items: [item, { ...item, id: "other", criterion: "review-requested", provider_name: "GitHub" }],
      auto_create: {
        enabled: false,
        rules: [{ provider_id: "github", criterion: "assigned-to-me" }],
        last_error: null,
      },
    });
    updateSuggestionPolicy.mockReturnValue(update);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    const criterion = await screen.findByRole("checkbox", { name: "GitHub · assigned-to-me" });
    fireEvent.click(criterion);
    await waitFor(() => expect(updateSuggestionPolicy).toHaveBeenCalledTimes(1));
    expect(updateSuggestionPolicy.mock.results[0]?.value).toBe(update);
    await waitFor(() =>
      expect(
        (screen.getByRole("group", { name: "Reviewed providers and criteria" }) as HTMLFieldSetElement).disabled,
      ).toBe(true),
    );
    fireEvent.click(screen.getByRole("checkbox", { name: "GitHub · review-requested" }));
    expect(updateSuggestionPolicy).toHaveBeenCalledTimes(1);
    await act(async () => {
      resolveUpdate(true);
      await update;
    });
  });
  it("opens a linked session and only creates another on explicit request", async () => {
    fetchSuggestions.mockResolvedValue({
      ...snapshot,
      items: [
        {
          ...item,
          session_id: "existing-session",
          can_start: false,
          start_blocked_reason: "This exact source is already linked to a session",
        },
      ],
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

  it("ignores an initial profile snapshot that resolves after a newer profile snapshot", async () => {
    const { promise: initial, resolve: resolveInitial } = Promise.withResolvers<typeof snapshot>();
    const refreshed = { ...snapshot, items: [{ ...item, title: "New snapshot" }] };
    fetchSuggestions.mockImplementation((profile: string | undefined) =>
      profile === "work" ? initial : Promise.resolve(refreshed),
    );
    const props = { onPrepare: vi.fn(), onOpenSession: vi.fn() };
    const { rerender } = render(<SuggestionsView {...props} profile="work" />);
    rerender(<SuggestionsView {...props} profile="other" />);

    expect(await screen.findByText("New snapshot")).toBeTruthy();
    await act(async () => {
      resolveInitial(snapshot);
      await initial;
    });

    expect(screen.getByText("New snapshot")).toBeTruthy();
    expect(screen.queryByText("Fix session lifecycle")).toBeNull();
  });
  it("ignores an older overlapping snapshot response after a newer load", async () => {
    const { promise: initial, resolve: resolveInitial } = Promise.withResolvers<typeof snapshot>();
    const newer = { ...snapshot, items: [{ ...item, title: "New snapshot" }] };
    fetchSuggestions.mockReturnValueOnce(initial).mockResolvedValueOnce(newer);
    refreshSuggestions.mockResolvedValue(true);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);

    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(2));
    expect(await screen.findByText("New snapshot")).toBeTruthy();
    await act(async () => {
      resolveInitial(snapshot);
      await initial;
    });

    expect(screen.getByText("New snapshot")).toBeTruthy();
    expect(screen.queryByText("Fix session lifecycle")).toBeNull();
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
