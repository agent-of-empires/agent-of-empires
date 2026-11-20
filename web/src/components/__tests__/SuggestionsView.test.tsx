// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SuggestionsView } from "../SuggestionsView";
import { setServerDown } from "../../lib/connectionState";
import { toastBus } from "../../lib/toastBus";
import type { SessionResponse, SuggestionItem, SuggestionsSnapshot } from "../../lib/types";

const {
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  dismissSuggestion,
  startSuggestion,
  fetchProjects,
  fetchPresets,
} = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  prepareSuggestion: vi.fn(),
  refreshSuggestions: vi.fn(),
  dismissSuggestion: vi.fn(),
  startSuggestion: vi.fn(),
  fetchProjects: vi.fn(),
  fetchPresets: vi.fn(),
}));

vi.mock("../../lib/api", () => ({
  fetchAgents: () => Promise.resolve(null),
  startSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  dismissSuggestion,
  fetchSuggestionProjects: fetchProjects,
  fetchSuggestionPresets: fetchPresets,
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
  toastBus.handler = null;
  fetchPresets.mockResolvedValue([]);
});

async function open(
  snap: SuggestionsSnapshot | null,
  props: { readOnly?: boolean; initialDismissed?: boolean; sessions?: SessionResponse[] } = {},
) {
  fetchSuggestions.mockResolvedValue(snap);
  const onPrepare = vi.fn();
  const onOpenSession = vi.fn();
  render(<SuggestionsView onPrepare={onPrepare} onOpenSession={onOpenSession} {...props} />);
  await waitFor(() => expect(fetchSuggestions).toHaveBeenCalled());
  return Object.assign(onPrepare, { onOpenSession });
}

describe("SuggestionsView", () => {
  const startMenu = async (name: string) => {
    fireEvent.click(await screen.findByRole("button", { name: "Start options" }));
    fireEvent.click(screen.getByRole("menuitem", { name: new RegExp(`^${name}`) }));
  };

  it("the ▾ menu takes focus, moves with the arrow keys, and Esc or a pick returns focus to the button", async () => {
    prepareSuggestion.mockReturnValue(new Promise(() => {}));
    await open(snapshot());
    const trigger = await screen.findByRole("button", { name: "Start options" });
    fireEvent.click(trigger);
    const items = screen.getAllByRole("menuitem");
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0]!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[1]);
    fireEvent.keyDown(items[1]!, { key: "ArrowUp" });
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0]!, { key: "End" });
    expect(document.activeElement).toBe(items[items.length - 1]);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);

    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("menuitem", { name: "Copy prompt" }));
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("Start runs the kind's preset, toasts the Session with Open, and opens no wizard", async () => {
    startSuggestion.mockResolvedValue({ kind: "started", sessionId: "s1", title: "Fix lifecycle" });
    const infoAction = vi.fn();
    toastBus.handler = { push: vi.fn(), error: vi.fn(), info: vi.fn(), openLink: vi.fn(), action: vi.fn(), infoAction };
    const onPrepare = await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    await waitFor(() => expect(infoAction).toHaveBeenCalled());
    expect(startSuggestion).toHaveBeenCalledWith("github", "issue-1", {});
    expect(infoAction.mock.calls[0]![0]).toBe("Started \u201cFix lifecycle\u201d");
    infoAction.mock.calls[0]![2]();
    expect(onPrepare.onOpenSession).toHaveBeenCalledWith("s1");
    expect(onPrepare).not.toHaveBeenCalled();
    expect(prepareSuggestion).not.toHaveBeenCalled();
  });

  it("other rows stay usable while one starts", async () => {
    startSuggestion.mockReturnValue(new Promise(() => {}));
    await open(snapshot({ items: [item, { ...item, id: "issue-2", title: "Second" }] }));
    const starts = await screen.findAllByRole("button", { name: "Start" });
    fireEvent.click(starts[0]!);
    expect(await screen.findByRole("button", { name: "Starting…" })).toHaveProperty("disabled", true);
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("lists the MCP servers in the hooks review, and a trust_changed answer asks again without approval", async () => {
    startSuggestion
      .mockResolvedValueOnce({
        kind: "hooks_need_trust",
        review: {
          commands: { on_create: ["npm ci"] },
          needs_mcp_trust: true,
          mcp_summaries: ["docs: npx docs-mcp --token ***"],
          review_fingerprint: "fp-1",
        },
      })
      .mockResolvedValueOnce({ kind: "trust_changed" })
      .mockResolvedValueOnce({
        kind: "hooks_need_trust",
        review: { commands: { on_create: ["npm ci", "make"] }, review_fingerprint: "fp-2" },
      });
    await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByText("docs: npx docs-mcp --token ***")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Approve and start" }));
    expect(await screen.findByText("make")).toBeTruthy();
    expect(startSuggestion).toHaveBeenLastCalledWith("github", "issue-1", {});
  });

  it("the ▾ menu summarises what the server says Start will run for the item", async () => {
    fetchSuggestions.mockResolvedValue(
      snapshot({
        items: [
          {
            ...item,
            resolved_project: { name: "aoe", path: "/work/aoe" },
            start: {
              agent: "claude",
              view: "terminal",
              worktree: false,
              sandbox: true,
              project: { name: "aoe", path: "/work/aoe" },
            },
          },
        ],
      }),
    );
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: "Start options" }));
    expect(screen.getByText("aoe · claude · Terminal view · sandbox")).toBeTruthy();
  });

  it("a repo with hooks pauses on the review, and Approve and start repeats the call with the fingerprint", async () => {
    startSuggestion
      .mockResolvedValueOnce({
        kind: "hooks_need_trust",
        review: {
          commands: { on_create: ["npm ci"], on_launch: ["./scripts/dev-setup.sh"] },
          review_fingerprint: "fp-1",
        },
      })
      .mockResolvedValueOnce({ kind: "started", sessionId: "s1", title: "Fix lifecycle" });
    await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByText("Review this repo's hooks")).toBeTruthy();
    expect(screen.getByText("npm ci")).toBeTruthy();
    expect(screen.getByText("./scripts/dev-setup.sh")).toBeTruthy();
    expect(startSuggestion).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Approve and start" }));
    await waitFor(() => expect(startSuggestion).toHaveBeenCalledTimes(2));
    expect(startSuggestion).toHaveBeenLastCalledWith("github", "issue-1", {
      approve_hooks: true,
      review_fingerprint: "fp-1",
    });
    await waitFor(() => expect(screen.queryByText("Review this repo's hooks")).toBeNull());
  });

  it("Cancel on the hooks review starts nothing", async () => {
    startSuggestion.mockResolvedValue({
      kind: "hooks_need_trust",
      review: { commands: { on_create: ["npm ci"] }, review_fingerprint: "fp-1" },
    });
    await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Review this repo's hooks")).toBeNull();
    expect(startSuggestion).toHaveBeenCalledTimes(1);
  });

  it("a failed start says why, then what to do, and Try again repeats the same start", async () => {
    startSuggestion.mockResolvedValueOnce({
      kind: "failed",
      reason: "Docker isn't running",
      hint: "Start it, then try again.",
      retryable: true,
    });
    const onPrepare = await open(snapshot());
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByText("Couldn't start: Docker isn't running. Start it, then try again.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Start" })).toBeNull();

    startSuggestion.mockResolvedValueOnce({ kind: "started", sessionId: "s1", title: "x" });
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(startSuggestion).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.queryByText(/Couldn't start/)).toBeNull());
    expect(onPrepare).not.toHaveBeenCalled();
  });

  it("Start with changes opens the prefilled New session dialog and keeps the link", async () => {
    prepareSuggestion.mockResolvedValue({
      ok: true,
      data: {
        title: "Fix lifecycle",
        initial_prompt: "Review the issue.",
        source_ref: "example/1",
        project: { name: "aoe", path: "/work/aoe" },
        tool: "opencode",
        view: "terminal",
        yolo: true,
        sandbox: false,
        worktree: true,
      },
    });
    const onPrepare = await open(snapshot());
    await startMenu("Start with changes");
    await waitFor(() =>
      expect(onPrepare).toHaveBeenCalledWith({
        source: {
          label: "GitHub",
          title: "Fix lifecycle",
          url: "https://example.test/issues/1",
          kindLabel: "Assigned issues",
        },
        template: { providerId: "github", criterion: "assigned" },
        title: "Fix lifecycle",
        path: "/work/aoe",
        initialTab: undefined,
        tool: "opencode",
        useStructuredView: false,
        yoloMode: true,
        sandboxEnabled: false,
        initialPrompt: "Review the issue.",
        suggestion: { provider_id: "github", id: "issue-1" },
      }),
    );
    expect(startSuggestion).not.toHaveBeenCalled();
  });

  it("a Start that loses the link to another start turns the row into Open", async () => {
    startSuggestion.mockResolvedValue({ kind: "already_started", sessionId: "s9" });
    await open(snapshot());
    fetchSuggestions.mockResolvedValue(
      snapshot({
        items: [
          {
            ...item,
            started_session: {
              id: "s9",
              title: "x",
              status: "Running",
              archived: false,
              created_at: new Date().toISOString(),
            },
          },
        ],
      }),
    );
    fireEvent.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByRole("button", { name: "Open" })).toBeTruthy();
  });

  it("a repo with no Project offers Start in…, and picking one starts with that Project", async () => {
    fetchProjects.mockResolvedValue([{ name: "website", path: "/w/site", origin: null }]);
    startSuggestion.mockResolvedValue({ kind: "started", sessionId: "s1", title: "Fix lifecycle" });
    await open(snapshot({ items: [{ ...item, mapping_key: "acme/web", resolved_project: null }] }));
    expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
    fireEvent.click(await screen.findByRole("button", { name: /^Start in…/ }));
    // Projects load after the menu opens; focus still moves to the first one.
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("menuitem", { name: "website" })));
    fireEvent.click(screen.getByRole("menuitem", { name: "website" }));
    await waitFor(() =>
      expect(startSuggestion).toHaveBeenCalledWith("github", "issue-1", { project: { path: "/w/site" } }),
    );
  });

  it("Start in… is off where the link cannot be saved", async () => {
    fetchSuggestions.mockResolvedValue(
      snapshot({ items: [{ ...item, mapping_key: "acme/web", resolved_project: null }] }),
    );
    render(<SuggestionsView canLink={false} onPrepare={vi.fn()} onOpenSession={vi.fn()} />);
    expect(((await screen.findByRole("button", { name: /^Start in…/ })) as HTMLButtonElement).disabled).toBe(true);
  });

  it("the ▾ menu says what Start uses and offers the kind's preset", async () => {
    const onEditPreset = vi.fn();
    fetchSuggestions.mockResolvedValue(
      snapshot({
        items: [
          {
            ...item,
            resolved_project: { name: "aoe", path: "/work/aoe" },
          },
        ],
      }),
    );
    const preset = { agent: "claude", view: "structured", worktree: true, sandbox: false, yolo: false, prompt: null };
    fetchPresets.mockResolvedValue([
      { provider_id: "github", criterion: "assigned", customized: false, preset, defaults: preset },
    ]);
    render(<SuggestionsView onPrepare={vi.fn()} onOpenSession={vi.fn()} onEditPreset={onEditPreset} />);
    await waitFor(() => expect(fetchPresets).toHaveBeenCalled());
    fireEvent.click(await screen.findByRole("button", { name: "Start options" }));
    expect(screen.getByText("aoe · claude · Structured view · worktree")).toBeTruthy();
    fireEvent.click(screen.getByRole("menuitem", { name: /Edit the Assigned issues preset/ }));
    expect(onEditPreset).toHaveBeenCalledWith("github", "assigned");
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
      const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
      expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
      expect(row.querySelector("p:nth-of-type(2)")?.textContent).toBe("example/1 · ⠋ Running · started 3 min ago");
      fireEvent.click(screen.getByRole("button", { name: "Open" }));
      expect(onPrepare.onOpenSession).toHaveBeenCalledWith("sess-1");
    });

    it("follows the live Session state, not the one captured at start", async () => {
      await open(snapshot({ items: [{ ...item, started_session: { ...started, status: "Starting" } }] }), {
        sessions: [{ id: "sess-1", status: "Idle" } as SessionResponse],
      });
      const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
      expect(row.querySelector("p:nth-of-type(2)")?.textContent).toBe("example/1 · ⠒ Idle · started 3 min ago");
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
      expect(row.textContent).toContain("Archived · started");
      cleanup();
      await open(snapshot({ items: [{ ...item, started_session: started }] }), { readOnly: true });
      fireEvent.click(await screen.findByRole("button", { name: "More" }));
      expect(screen.queryByRole("menuitem", { name: "Dismiss" })).toBeNull();
      expect(screen.getByRole("button", { name: "Open" })).toBeTruthy();
    });
  });

  it("renders one compact row: title, short reference, reason once, with no preview, source name or link", async () => {
    await open(snapshot());
    const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
    expect(row.textContent).not.toContain("UNTRUSTED BODY");
    expect(row.querySelector("a")).toBeNull();
    expect(row.querySelector("p:nth-of-type(1)")?.textContent).toBe("Fix lifecycle");
    const meta = row.querySelector("p:nth-of-type(2)") as HTMLElement;
    expect(meta.textContent).toBe("example/1 · Assigned to you");
    expect(meta.textContent).not.toContain("GitHub");
    expect(meta.firstElementChild?.className).toContain("font-mono");
  });

  it("shows #N in a repo group instead of owner/repo#N", async () => {
    await open(snapshot({ items: [{ ...item, source_ref: "acme/web#12", mapping_key: "acme/web" }] }));
    const row = (await screen.findByText("Fix lifecycle")).closest("li") as HTMLElement;
    expect(row.querySelector("p:nth-of-type(2)")?.textContent).toBe("#12 · Assigned to you");
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
      expect((await screen.findByText("Fix lifecycle")).className).toContain("opacity-[0.55]");
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

    it("Start with changes on a failed row opens the dialog and clears the failure", async () => {
      startSuggestion.mockResolvedValueOnce({
        kind: "failed",
        reason: "worktree branch already exists",
        retryable: false,
      });
      const onPrepare = await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Start" }));
      expect(await screen.findByText(/Couldn't start: worktree branch already exists\./)).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
      expect(screen.queryByRole("alert")).toBeNull();
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
      fireEvent.click(screen.getByRole("button", { name: "Start with changes" }));
      await waitFor(() => expect(onPrepare).toHaveBeenCalledTimes(1));
      expect(onPrepare.mock.calls[0]![0].source.failure).toEqual({
        reason: "worktree branch already exists",
        hint: undefined,
      });
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

    it("groups by repo with counts, no-repo last, and collapses from the header", async () => {
      const base = two();
      await open({
        ...base,
        items: [
          { ...base.items[0]!, mapping_key: undefined },
          { ...base.items[1]!, mapping_key: "acme/web" },
          { ...base.items[2]!, mapping_key: "acme/web" },
        ],
      });
      const headings = await screen.findAllByRole("heading", { level: 2 });
      expect(headings.map((h) => h.textContent)).toEqual(["web 2", "Other 1"]);
      const web = screen.getByRole("region", { name: "Repo web" });
      expect(web.textContent).toContain("In api");
      fireEvent.click(within(web).getByRole("button", { name: /web/ }));
      expect(screen.queryByText("In api")).toBeNull();
      expect(screen.getByText("Loose")).toBeTruthy();
    });

    it("chips filter by provider and show the active counts", async () => {
      await open(two());
      expect((await screen.findByRole("button", { name: "All 3" })).getAttribute("aria-pressed")).toBe("true");
      fireEvent.click(screen.getByRole("button", { name: "Linear 1" }));
      expect(screen.queryByText("In api")).toBeNull();
      expect(screen.getByText("Linear api")).toBeTruthy();
    });

    it("drops a repo filter when the provider tab changes, so the other list is not left empty", async () => {
      const base = two();
      await open({
        ...base,
        items: [
          { ...base.items[0]!, title: "GitHub item", mapping_key: "acme/web" },
          { ...base.items[2]!, title: "Linear item", mapping_key: "team/api" },
        ],
      });
      await screen.findByText("GitHub item");
      fireEvent.click(screen.getByRole("button", { name: "All repos" }));
      fireEvent.click(screen.getAllByRole("button", { name: /^web/ })[0]!);
      expect(screen.queryByText("Linear item")).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Linear 1" }));
      expect(screen.getByText("Linear item")).toBeTruthy();
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
      expect(screen.getByRole("list", { name: "Provider status" }).textContent).toContain("Linear updated 2 min ago");
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
    startSuggestion.mockResolvedValue({ kind: "stale" });
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
    fireEvent.click(await screen.findByRole("button", { name: "Dismissed 0" }));
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
    await startMenu("Start with changes");
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
      startSuggestion.mockResolvedValue({ kind: "failed", reason: "boom", retryable: true });
      const onPrepare = await open(snapshot());
      fireEvent.click(await screen.findByRole("button", { name: "Start" }));
      expect(await screen.findByText("Couldn't start: boom.")).toBeTruthy();
      for (const name of ["Try again", "Dismiss"]) {
        await waitFor(() => expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(false));
      }
      expect(onPrepare).not.toHaveBeenCalled();
    });
  });

  it("filters by search and by org or repo chips regardless of project mapping", async () => {
    const at = (id: string, title: string, mapping_key: string) => ({ ...item, id, title, mapping_key, project: null });
    await open(
      snapshot({
        items: [at("1", "Web bug", "acme/web"), at("2", "Api bug", "acme/api"), at("3", "Tool chore", "other/tool")],
      }),
    );
    await screen.findByText("Web bug");
    const titles = () => ["Web bug", "Api bug", "Tool chore"].filter((t) => screen.queryByText(t));
    fireEvent.click(screen.getByRole("button", { name: "All repos" }));
    const menu = (name: RegExp) => screen.getByRole("button", { name });

    fireEvent.click(menu(/^acme \(org\)/));
    expect(titles()).toEqual(["Web bug", "Api bug"]);
    fireEvent.click(menu(/^tool/));
    expect(titles()).toEqual(["Web bug", "Api bug", "Tool chore"]);
    fireEvent.click(menu(/^acme \(org\)/));
    expect(titles()).toEqual(["Tool chore"]);

    fireEvent.click(screen.getByRole("button", { name: "Clear filters" }));
    fireEvent.change(screen.getByLabelText("Search suggestions"), { target: { value: "API" } });
    expect(titles()).toEqual(["Api bug"]);
    fireEvent.change(screen.getByLabelText("Search suggestions"), { target: { value: "nothing" } });
    expect(screen.getByText("No suggestions match the filters.")).toBeTruthy();
  });
});
