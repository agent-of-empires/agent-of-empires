// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsView } from "../SuggestionsView";
import { setServerDown } from "../../lib/connectionState";
import type { SuggestionItem, SuggestionsSnapshot } from "../../lib/types";

const {
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  dismissSuggestion,
  createSession,
  fetchCreateBootId,
  pending,
} = vi.hoisted(() => ({
  createSession: vi.fn(),
  fetchCreateBootId: vi.fn(),
  fetchSuggestions: vi.fn(),
  prepareSuggestion: vi.fn(),
  refreshSuggestions: vi.fn(),
  dismissSuggestion: vi.fn(),
  pending: { peek: vi.fn(), register: vi.fn(), release: vi.fn(), resolve: vi.fn() },
}));

vi.mock("../../lib/api", () => ({
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  dismissSuggestion,
  createSession,
  fetchCreateBootId,
}));

vi.mock("../../lib/pendingCreates", () => ({
  newIdempotencyKey: () => "key-1",
  peekPendingCreate: pending.peek,
  registerPendingCreate: pending.register,
  releasePendingCreate: pending.release,
  resolvePendingCreate: pending.resolve,
}));

const item: SuggestionItem = {
  id: "issue-1",
  provider_id: "github",
  title: "Fix lifecycle",
  reason: "Assigned to you",
  source_url: "https://example.test/issues/1",
  source_ref: "example/1",
  preview: "UNTRUSTED BODY",
  project: "aoe",
  criterion: "assigned",
  draft: { title: "Fix lifecycle", prompt: "Review the issue." },
  dismissed: false,
};
const snapshot = (over: Partial<SuggestionsSnapshot> = {}): SuggestionsSnapshot => ({
  items: [item],
  providers: [
    {
      id: "github",
      name: "GitHub",
      status: "ok",
      enabled: true,
      sources: [
        { criterion: "assigned", label: "Assigned issues", description: "", enabled: true, variables: [] },
        { criterion: "ci", label: "Failing checks", description: "", enabled: false, variables: [] },
      ],
    },
  ],
  prefs: {
    homepage: true,
    sidebar: false,
    refresh_minutes: 15,
    start_without_review: false,
    show_source_ref: true,
    provider_order: ["github"],
  },
  ...over,
});
const ok = { ok: true, data: undefined };

afterEach(() => {
  cleanup();
  setServerDown(false);
  vi.clearAllMocks();
  pending.peek.mockReturnValue(null);
  fetchCreateBootId.mockResolvedValue("boot-1");
});

async function open(snap: SuggestionsSnapshot | null, props: { readOnly?: boolean; initialDismissed?: boolean } = {}) {
  fetchSuggestions.mockResolvedValue(snap);
  const onPrepare = vi.fn();
  const onOpenSession = vi.fn();
  render(<SuggestionsView onPrepare={onPrepare} onOpenSession={onOpenSession} {...props} />);
  await waitFor(() => expect(fetchSuggestions).toHaveBeenCalled());
  return Object.assign(onPrepare, { onOpenSession });
}

const prepared = {
  title: "Fix lifecycle",
  initial_prompt: "Template prompt.",
  templated: true,
  source_ref: "example/1",
  project: { name: "aoe", path: "/work/aoe" },
  tool: "opencode",
  yolo: true,
  sandbox: false,
  worktree: true,
};

async function startWithoutReview(data: Partial<typeof prepared> | { project: null }, created?: object) {
  prepareSuggestion.mockResolvedValue({ ok: true, data: { ...prepared, ...data } });
  if (created) createSession.mockResolvedValue(created);
  const snap = snapshot();
  fetchSuggestions.mockResolvedValue({ ...snap, prefs: { ...snap.prefs, start_without_review: true } });
  const onPrepare = vi.fn();
  const onStarted = vi.fn();
  render(<SuggestionsView onPrepare={onPrepare} onStarted={onStarted} />);
  fireEvent.click(await screen.findByRole("button", { name: "Start" }));
  return { onPrepare, onStarted };
}

describe("SuggestionsView", () => {
  it("Start prepares and hands the draft prompt to the wizard without creating anything", async () => {
    prepareSuggestion.mockResolvedValue({
      ok: true,
      data: {
        title: "Fix lifecycle",
        initial_prompt: "Review the issue.",
        templated: false,
        source_ref: "example/1",
        project: { name: "aoe", path: "/work/aoe" },
        tool: "opencode",
        yolo: true,
        sandbox: false,
        worktree: true,
      },
    });
    const onPrepare = await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    await waitFor(() =>
      expect(onPrepare).toHaveBeenCalledWith({
        source: { label: "GitHub", title: "Fix lifecycle", url: "https://example.test/issues/1" },
        title: "Fix lifecycle",
        path: "/work/aoe",
        tool: "opencode",
        yoloMode: true,
        sandboxEnabled: false,
        initialPrompt: "Review the issue.",
        suggestion: { provider_id: "github", id: "issue-1" },
      }),
    );
    expect(prepareSuggestion).toHaveBeenCalledWith("github", "issue-1");
  });

  describe("a started suggestion", () => {
    const started = {
      id: "sess-1",
      title: "Review RFC 4562",
      status: "Running" as const,
      archived: false,
      created_at: new Date(Date.now() - 3 * 60_000).toISOString(),
    };
    const prepared = {
      ok: true,
      data: {
        title: "Fix lifecycle",
        initial_prompt: "Review the issue.",
        source_ref: "example/1",
        project: { name: "aoe", path: "/work/aoe" },
        tool: "opencode",
        yolo: false,
        sandbox: false,
        worktree: true,
      },
    };
    const openStarted = (session = started) => open(snapshot({ items: [{ ...item, started_session: session }] }));

    it("shows Open and the Session state instead of Start", async () => {
      const onPrepare = await openStarted();
      expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
      const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
      expect(row.querySelector("p:nth-of-type(2)")?.textContent).toBe(
        "GitHub · example/1 · ⠋ Session running · Review RFC 4562 · started 3 min ago · aoe",
      );
      fireEvent.click(screen.getByRole("button", { name: "Open" }));
      expect(onPrepare.onOpenSession).toHaveBeenCalledWith("sess-1");
    });

    it("Start another opens the wizard without linking the new Session", async () => {
      prepareSuggestion.mockResolvedValue(prepared);
      const onPrepare = await openStarted();
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      expect(screen.getByRole("menuitem", { name: "Open in GitHub ↗" }).getAttribute("href")).toBe(
        "https://example.test/issues/1",
      );
      fireEvent.click(screen.getByRole("menuitem", { name: "Start another Session…" }));
      await waitFor(() => expect(onPrepare).toHaveBeenCalled());
      expect(onPrepare.mock.calls[0]![0].suggestion).toBeUndefined();
    });

    it("an archived Session says so and read-only keeps Open but offers no Dismiss", async () => {
      await openStarted({ ...started, archived: true });
      const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
      expect(row.textContent).toContain("Archived · Review RFC 4562");
      cleanup();
      await open(snapshot({ items: [{ ...item, started_session: started }] }), { readOnly: true });
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      expect(screen.queryByRole("menuitem", { name: "Dismiss" })).toBeNull();
      expect(screen.getByRole("button", { name: "Open" })).toBeTruthy();
    });
  });

  it("Start without review creates the session from the prepared draft and skips the wizard", async () => {
    const session = { id: "s1" };
    const { onPrepare, onStarted } = await startWithoutReview({}, { ok: true, session });
    await waitFor(() => expect(onStarted).toHaveBeenCalledWith(session));
    expect(createSession).toHaveBeenCalledWith({
      title: "Fix lifecycle",
      path: "/work/aoe",
      tool: "opencode",
      yolo_mode: true,
      sandbox: false,
      worktree_enabled: true,
      initial_prompt: "Template prompt.",
      suggestion: { provider_id: "github", id: "issue-1" },
      idempotency_key: "key-1",
    });
    expect(pending.register).toHaveBeenCalledWith(expect.objectContaining({ tool: "opencode", origin: "boot-1" }), {
      claimed: true,
    });
    expect(pending.resolve).toHaveBeenCalledWith("key-1");
    expect(onPrepare).not.toHaveBeenCalled();
  });

  it("Start without review falls back to the wizard when the draft is unresolved or hooks need trust", async () => {
    for (const [data, created] of [
      [{ project: null }, undefined],
      [{ initial_prompt: "" }, undefined],
      [{ templated: false }, undefined],
      [{}, { ok: false, error: "hooks", hooksNeedTrust: {} }],
      [{}, { ok: false, error: "changed", trustChanged: true }],
    ] as const) {
      cleanup();
      vi.clearAllMocks();
      const { onPrepare, onStarted } = await startWithoutReview(data, created);
      await waitFor(() => expect(onPrepare).toHaveBeenCalled());
      expect(onStarted).not.toHaveBeenCalled();
      expect(createSession).toHaveBeenCalledTimes(created ? 1 : 0);
      if (created) expect(pending.resolve).toHaveBeenCalledWith("key-1");
    }
  });

  it("Start without review shows a notice and never reopens the wizard when the outcome is unknown or the create fails", async () => {
    for (const created of [
      { ok: false, error: "No answer from the server (502)", network: true },
      { ok: false, error: "Unknown outcome", outcomeUnknown: true },
      { ok: false, error: "Sandbox unavailable" },
    ]) {
      cleanup();
      vi.clearAllMocks();
      const { onPrepare, onStarted } = await startWithoutReview({}, created);
      expect((await screen.findByRole("alert")).textContent).toBe(created.error);
      expect(onPrepare).not.toHaveBeenCalled();
      expect(onStarted).not.toHaveBeenCalled();
      const unresolved = "network" in created || "outcomeUnknown" in created;
      expect(pending.release).toHaveBeenCalledTimes(unresolved ? 1 : 0);
      expect(pending.resolve).toHaveBeenCalledTimes(unresolved ? 0 : 1);
    }
  });

  it("Start without review refuses a re-click while an earlier create is unresolved", async () => {
    pending.peek.mockReturnValue({ body: { idempotency_key: "old" } });
    const { onPrepare } = await startWithoutReview({});
    expect((await screen.findByRole("alert")).textContent).toContain("still being created");
    expect(createSession).not.toHaveBeenCalled();
    expect(onPrepare).not.toHaveBeenCalled();
  });

  it("renders one compact row: title, then Source, reference, reason and Project, with no preview or chip", async () => {
    await open(snapshot());
    const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
    expect(row.textContent).not.toContain("UNTRUSTED BODY");
    expect(row.querySelector("a")).toBeNull();
    const meta = row.querySelector("p:nth-of-type(2)") as HTMLElement;
    expect(meta.textContent).toBe("GitHub · example/1 · Assigned to you · aoe");
    expect(meta.firstElementChild?.className).toContain("font-mono");
  });

  it("opens on the Dismissed filter when asked", async () => {
    await open(snapshot({ items: [{ ...item, dismissed: true }] }), { initialDismissed: true });
    expect(await screen.findByText("Fix lifecycle")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Restore" })).toBeTruthy();
  });

  it("dismiss, then restore from the dismissed list", async () => {
    dismissSuggestion.mockResolvedValue(ok);
    await open(snapshot());
    const button = await screen.findByRole("button", { name: "Dismiss" });
    fetchSuggestions.mockResolvedValue(snapshot({ items: [{ ...item, dismissed: true }] }));
    fireEvent.click(button);
    await waitFor(() => expect(dismissSuggestion).toHaveBeenCalledWith("github", "issue-1", true));
    fireEvent.click(await screen.findByRole("button", { name: "Dismissed 1" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore" }));
    await waitFor(() => expect(dismissSuggestion).toHaveBeenCalledWith("github", "issue-1", false));
  });

  describe("edge states", () => {
    it("a stale item is dimmed and can only be dismissed", async () => {
      await open(snapshot({ items: [{ ...item, stale: true }] }));
      const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
      expect(row.querySelector("p")?.className).toContain("opacity-[0.55]");
      expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
      expect(screen.getByRole("button", { name: "Dismiss" })).toBeTruthy();
    });

    it("a stale started item cannot start another Session", async () => {
      await open(
        snapshot({
          items: [
            {
              ...item,
              stale: true,
              started_session: {
                id: "sess-1",
                title: "Review",
                status: "Idle",
                archived: false,
                created_at: new Date().toISOString(),
              },
            },
          ],
        }),
      );
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      expect((screen.getByRole("menuitem", { name: "Start another Session…" }) as HTMLButtonElement).disabled).toBe(
        true,
      );
      expect(screen.getByRole("button", { name: "Open" })).toBeTruthy();
    });

    it("Start another failing keeps Open, and the retry is unlinked too", async () => {
      const started = {
        id: "sess-1",
        title: "Review",
        status: "Running" as const,
        archived: false,
        created_at: new Date().toISOString(),
      };
      prepareSuggestion.mockResolvedValueOnce({ ok: false, error: "no agent", stale: false });
      const onPrepare = await open(snapshot({ items: [{ ...item, started_session: started }] }));
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      fireEvent.click(screen.getByRole("menuitem", { name: "Start another Session…" }));
      expect(await screen.findByText(/Couldn't start: no agent/)).toBeTruthy();
      expect(screen.getByRole("button", { name: "Open" })).toBeTruthy();

      prepareSuggestion.mockResolvedValueOnce({
        ok: true,
        data: {
          title: "Fix lifecycle",
          initial_prompt: "p",
          source_ref: "example/1",
          project: { name: "aoe", path: "/work/aoe" },
          tool: "claude",
          yolo: false,
          sandbox: false,
          worktree: true,
        },
      });
      fireEvent.click(screen.getByRole("button", { name: "More" }));
      fireEvent.click(screen.getByRole("menuitem", { name: "Start another Session…" }));
      await waitFor(() => expect(onPrepare).toHaveBeenCalledTimes(1));
      expect(onPrepare.mock.calls[0]![0].suggestion).toBeUndefined();
    });

    it("a failed start stays on its row with Open wizard as the only way to retry", async () => {
      prepareSuggestion.mockResolvedValueOnce({ ok: false, error: "worktree branch already exists", stale: false });
      const onPrepare = await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Start" }));
      expect(await screen.findByText(/Couldn't start: worktree branch already exists/)).toBeTruthy();
      expect(screen.queryByRole("alert")).toBeNull();
      expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
      expect(onPrepare).not.toHaveBeenCalled();

      prepareSuggestion.mockResolvedValueOnce({
        ok: true,
        data: {
          title: "Fix lifecycle",
          initial_prompt: "p",
          source_ref: "example/1",
          project: { name: "aoe", path: "/work/aoe" },
          tool: "claude",
          yolo: false,
          sandbox: false,
          worktree: true,
        },
      });
      fireEvent.click(screen.getByRole("button", { name: "Open wizard" }));
      await waitFor(() => expect(onPrepare).toHaveBeenCalledTimes(1));
      await waitFor(() => expect(screen.queryByText(/Couldn't start/)).toBeNull());
    });

    it("a disabled provider is explained with a way to turn it on", async () => {
      const base = snapshot();
      const onOpenSettings = vi.fn();
      fetchSuggestions.mockResolvedValue({
        ...base,
        items: [],
        providers: [...base.providers, { id: "linear", name: "Linear", status: "ok", enabled: false, sources: [] }],
      });
      render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} onOpenSettings={onOpenSettings} />);
      expect(
        await screen.findByText(/Linear suggestions are hidden because Linear is off in Settings › Suggestions\./),
      ).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Turn it on" }));
      expect(onOpenSettings).toHaveBeenCalled();
    });
  });

  describe("provider text is untrusted", () => {
    const hostile = {
      ...item,
      title: "[click me](javascript:alert(1)) **bold**",
      reason: "<img src=x onerror=alert(1)> _em_ https://evil.test/x",
      project: "<b>proj</b>",
      source_url: "javascript:alert(1)",
      preview: "# heading\n[link](https://evil.test)",
      started_session: {
        id: "s1",
        title: "<script>1</script> [t](https://evil.test)",
        status: "Running" as const,
        archived: false,
        created_at: new Date().toISOString(),
      },
    };

    it("renders title, reason, project and session title as literal text with no links", async () => {
      const plain = { ...hostile, id: "plain", started_session: undefined };
      await open(snapshot({ items: [plain, hostile] }));
      const [fresh, started] = (await screen.findAllByText(/click me/)).map((el) => el.closest("li") as HTMLElement);
      expect(fresh!.textContent).toContain("[click me](javascript:alert(1)) **bold**");
      expect(fresh!.textContent).toContain("<img src=x onerror=alert(1)> _em_ https://evil.test/x");
      expect(fresh!.textContent).toContain("<b>proj</b>");
      expect(started!.textContent).toContain("<script>1</script> [t](https://evil.test)");
      for (const row of [fresh!, started!]) {
        expect(row.querySelector("a, img, b, strong, em, script, h1")).toBeNull();
        expect(row.textContent).not.toContain("# heading");
      }
    });

    it("never links a source_url that is not http(s)", async () => {
      await open(snapshot({ items: [hostile] }));
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      expect(screen.queryByRole("menuitem", { name: /Open in/ })).toBeNull();
      expect(document.querySelector('a[href^="javascript"]')).toBeNull();
    });
  });

  describe("grouping, filters and provider status", () => {
    const linear = {
      id: "linear",
      name: "Linear",
      status: "ok" as const,
      enabled: true,
      updated_at: Math.floor(Date.now() / 1000) - 120,
      sources: [],
    };
    const resolved = (name: string) => ({ name, path: `/work/${name}` });
    const two = (): SuggestionsSnapshot => {
      const base = snapshot();
      return {
        ...base,
        providers: [{ ...base.providers[0]!, updated_at: Math.floor(Date.now() / 1000) - 120 }, linear],
        items: [
          { ...item, id: "a", title: "Loose", resolved_project: null },
          { ...item, id: "b", title: "In api", resolved_project: resolved("api") },
          { ...item, id: "c", provider_id: "linear", title: "Linear api", resolved_project: resolved("api") },
        ],
      };
    };

    it("groups by resolved project with counts, unresolved last", async () => {
      await open(two());
      const headings = await screen.findAllByRole("heading", { level: 2 });
      expect(headings.map((h) => h.textContent)).toEqual(["api 2", "No project 1"]);
      const api = screen.getByRole("region", { name: "Project api" });
      expect(api.textContent).toContain("In api");
      expect(api.textContent).toContain("Linear api");
    });

    it("chips filter by provider and show the active counts", async () => {
      await open(two());
      expect((await screen.findByRole("button", { name: "All 3" })).getAttribute("aria-pressed")).toBe("true");
      fireEvent.click(screen.getByRole("button", { name: "Linear 1" }));
      expect(screen.queryByText("In api")).toBeNull();
      expect(screen.getByText("Linear api")).toBeTruthy();
    });

    it("shows each provider's update age, error, emptiness and refresh state", async () => {
      const base = two();
      await open({
        ...base,
        providers: [
          { ...base.providers[0]!, refreshing: true },
          { ...linear, status: "error", error: "API key rejected" },
        ],
      });
      const status = await screen.findByRole("list", { name: "Provider status" });
      expect(status.textContent).toContain("⠙ GitHub: refreshing…");
      expect(status.textContent).toContain("Linear: couldn't refresh, API key rejected");
      cleanup();
      await open({ ...base, items: [{ ...base.items[0]!, provider_id: "linear" }] });
      expect((await screen.findByRole("list", { name: "Provider status" })).textContent).toContain(
        "GitHub: nothing new",
      );
      expect(screen.getByRole("list", { name: "Provider status" }).textContent).toContain("Linear: updated 2 min ago");
    });

    it("follows a refreshing provider until it answers", async () => {
      const base = two();
      fetchSuggestions
        .mockResolvedValueOnce({ ...base, providers: [{ ...base.providers[0]!, refreshing: true }, linear] })
        .mockResolvedValueOnce(null)
        .mockResolvedValue(base);
      render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);
      expect((await screen.findByRole("list", { name: "Provider status" })).textContent).toContain("refreshing…");
      // The second fetch fails; the poll still comes back for a third.
      await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(3), { timeout: 8000 });
      await waitFor(() =>
        expect(screen.getByRole("list", { name: "Provider status" }).textContent).not.toContain("refreshing…"),
      );
    }, 15_000);

    it("a dismiss offers Undo here too", async () => {
      dismissSuggestion.mockResolvedValue(ok);
      await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
      expect(await screen.findByText(/Dismissed/)).toBeTruthy();
      expect(screen.queryByRole("button", { name: /Stop suggesting/ })).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Undo" }));
      await waitFor(() => expect(dismissSuggestion).toHaveBeenCalledWith("github", "issue-1", false));
    });
  });

  it("a stale item shows a notice and reloads instead of opening the wizard", async () => {
    prepareSuggestion.mockResolvedValue({ ok: false, error: "gone", stale: true });
    const onPrepare = await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Refreshing the list");
    expect(onPrepare).not.toHaveBeenCalled();
    expect(fetchSuggestions).toHaveBeenCalledTimes(2);
  });

  it("refresh calls the endpoint and reloads", async () => {
    refreshSuggestions.mockResolvedValue(ok);
    await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(refreshSuggestions).toHaveBeenCalled());
    await waitFor(() => expect(fetchSuggestions).toHaveBeenCalledTimes(2));
  });

  it("shows provider errors, empty, and load-failure states", async () => {
    await open(
      snapshot({
        items: [],
        providers: [
          { id: "github", name: "GitHub", status: "error", error: "token expired", enabled: true, sources: [] },
        ],
      }),
    );
    expect(await screen.findByText(/token expired/)).toBeTruthy();
    expect(screen.getByText("Nothing to suggest right now.")).toBeTruthy();
    cleanup();

    await open(null);
    expect((await screen.findByRole("alert")).textContent).toContain("Could not load");
  });

  it("read-only disables Start and Refresh, drops Dismiss and explains why", async () => {
    await open(snapshot(), { readOnly: true });
    await screen.findByText("Fix lifecycle");
    const start = screen.getByRole("button", { name: "Start" }) as HTMLButtonElement;
    expect(start.disabled).toBe(true);
    expect(start.title).toBe("Read-only: can't create Sessions");
    expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
    expect(screen.getByText("This dashboard is read-only. Start, Dismiss and Refresh are off.")).toBeTruthy();
  });

  it("disables every mutation while offline", async () => {
    await open(snapshot());
    await screen.findByText("Fix lifecycle");
    act(() => setServerDown(true));
    for (const name of ["Start", "Dismiss", "Refresh"]) {
      expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true);
    }
    expect(
      screen.getByText("Disconnected. Suggestions show from the last fetch; reconnect to start one."),
    ).toBeTruthy();
  });

  it("restoring the last dismissed item keeps the way back to the active list", async () => {
    dismissSuggestion.mockResolvedValue(ok);
    await open(snapshot({ items: [{ ...item, dismissed: true }] }));
    fireEvent.click(await screen.findByRole("button", { name: "Dismissed 1" }));
    fetchSuggestions.mockResolvedValue(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Restore" }));
    fireEvent.click(await screen.findByRole("button", { name: /^All/ }));
    expect(await screen.findByText("Fix lifecycle")).toBeTruthy();
  });

  it("omits an empty initial prompt from the wizard prefill", async () => {
    prepareSuggestion.mockResolvedValue({
      ok: true,
      data: {
        title: "t",
        initial_prompt: "",
        source_ref: "example/1",
        project: null,
        tool: "claude",
        yolo: false,
        sandbox: false,
        worktree: false,
      },
    });
    const onPrepare = await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    await waitFor(() => expect(onPrepare).toHaveBeenCalled());
    expect(onPrepare.mock.calls[0][0].initialPrompt).toBeUndefined();
  });

  describe("non-stale failures show the error and release the controls", () => {
    const failure = { ok: false, error: "boom", stale: false };
    const expectReleased = async (...names: string[]) => {
      expect((await screen.findByRole("alert")).textContent).toContain("boom");
      for (const name of names) {
        await waitFor(() => expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(false));
      }
    };

    it("dismiss", async () => {
      dismissSuggestion.mockResolvedValue(failure);
      await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
      await expectReleased("Start", "Dismiss", "Refresh");
    });

    it("refresh", async () => {
      refreshSuggestions.mockResolvedValue(failure);
      await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Refresh" }));
      await expectReleased("Refresh", "Start");
    });

    it("start", async () => {
      prepareSuggestion.mockResolvedValue(failure);
      const onPrepare = await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Start" }));
      expect(await screen.findByText("Couldn't start: boom")).toBeTruthy();
      for (const name of ["Open wizard", "Dismiss"]) {
        await waitFor(() => expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(false));
      }
      expect(onPrepare).not.toHaveBeenCalled();
    });
  });
});
