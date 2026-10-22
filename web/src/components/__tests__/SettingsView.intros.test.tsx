// @vitest-environment jsdom
// Settings tab intros (#4594): shown until folded, folds kept in synced web UI state, reopen from "About".
// Persistence through the Daemon and the "Show tutorial" reset run live in tests/live/onboarding.spec.ts.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import { resetSettingsIntros } from "../../lib/settingsIntros";
import { hydrateWebUiStateFromServer } from "../../lib/webUiSync";
import * as api from "../../lib/api";

const SCHEMA = [
  {
    section: "worktree",
    field: "enabled",
    label: "Enabled by Default",
    widget: { kind: "toggle" },
    advanced: false,
    category: "worktree",
    description: "",
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
  },
];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve([{ name: "main", is_default: true }])),
  fetchSettings: vi.fn(() => Promise.resolve({ worktree: {}, sandbox: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  getWebUiState: vi.fn(),
}));

const KEY = (id: string) => `aoe-settings-intro-${id}`;

const renderTab = (tab: string, readOnly = false) =>
  render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={() => {}}
      onServerAboutRefresh={() => {}}
      readOnly={readOnly}
    />,
  );

const intro = () => screen.queryByRole("region", { name: "About this tab" });
const about = () => screen.queryByRole("button", { name: "About this tab" });

beforeEach(async () => {
  localStorage.clear();
  // Intros wait for this page load's hydration to settle; null is an unreachable Daemon.
  vi.mocked(api.getWebUiState).mockResolvedValue(null);
  await hydrateWebUiStateFromServer();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Settings tab intros", () => {
  it("opens with Details collapsed, folds into About on Got it, and reopens from About", async () => {
    renderTab("worktree");
    await waitFor(() => expect(intro()).not.toBeNull());
    expect(intro()?.textContent).toContain("own copy of the branch");
    expect(about()).toBeNull();

    const details = screen.getByRole("button", { name: "Details" });
    const detailText = screen.getByText(/path template expands/);
    expect(details.getAttribute("aria-expanded")).toBe("false");
    expect(detailText.hidden).toBe(true);
    fireEvent.click(details);
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(detailText.hidden).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Got it" }));
    expect(intro()).toBeNull();
    expect(localStorage.getItem(KEY("worktree"))).toBe("folded");

    fireEvent.click(about() as HTMLElement);
    expect(intro()).not.toBeNull();
    // Reopening is local to this visit; only "Show tutorial" unfolds the stored fold.
    expect(localStorage.getItem(KEY("worktree"))).toBe("folded");
  });

  it("unfolds in an open Settings view when Show tutorial resets the intros", async () => {
    localStorage.setItem(KEY("worktree"), "folded");
    renderTab("worktree");
    await waitFor(() => expect(about()).not.toBeNull());
    act(() => resetSettingsIntros(false));
    expect(intro()).not.toBeNull();
    expect(about()).toBeNull();
    // Written, not removed, so the reset reaches Devices that hydrate from the Daemon.
    for (const id of ["worktree", "plugins", "structured-view"]) {
      expect(localStorage.getItem(KEY(id)), id).toBe("unfolded");
    }
    // A hydration answered before the reset's write reached the Daemon must not fold it again.
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "folded" });
    await act(() => hydrateWebUiStateFromServer());
    expect(intro()).not.toBeNull();
  });

  it("re-reads the folds when the Daemon's web UI state arrives after the view mounted", async () => {
    renderTab("worktree");
    await waitFor(() => expect(intro()).not.toBeNull());
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "folded" });
    await act(() => hydrateWebUiStateFromServer());
    expect(intro()).toBeNull();
    expect(about()).not.toBeNull();
  });

  it("keeps an intro the user reopened open when the Daemon's same fold arrives later", async () => {
    localStorage.setItem(KEY("worktree"), "folded");
    renderTab("worktree");
    await waitFor(() => expect(about()).not.toBeNull());
    let respond: (state: Record<string, string>) => void = () => {};
    vi.mocked(api.getWebUiState).mockReturnValue(new Promise((resolve) => (respond = resolve)));
    const hydrating = hydrateWebUiStateFromServer();

    fireEvent.click(about() as HTMLElement);
    expect(intro()).not.toBeNull();
    await act(async () => {
      respond({ [KEY("worktree")]: "folded", [KEY("plugins")]: "folded" });
      await hydrating;
    });
    expect(intro()).not.toBeNull();
    expect(about()).toBeNull();
  });

  it("shows neither the intro nor About until this page load's hydration settles", async () => {
    vi.resetModules();
    const { SettingsView: FreshView } = await import("../SettingsView");
    const sync = await import("../../lib/webUiSync");
    localStorage.setItem(KEY("worktree"), "folded");
    let respond: (state: Record<string, string> | null) => void = () => {};
    vi.mocked(api.getWebUiState).mockReturnValue(new Promise((resolve) => (respond = resolve)));
    const hydrating = sync.hydrateWebUiStateFromServer();
    render(<FreshView onClose={() => {}} tab="worktree" onSelectTab={() => {}} onServerAboutRefresh={() => {}} />);
    await waitFor(() => expect(screen.queryByText(/Loading settings/)).toBeNull());
    expect(intro()).toBeNull();
    expect(about()).toBeNull();
    // An unreachable Daemon still settles it, so the stored fold shows.
    await act(async () => {
      respond(null);
      await hydrating;
    });
    expect(about()).not.toBeNull();
  });

  it("holds a replay made before the page's first hydration until it settles, with Settings closed", async () => {
    vi.resetModules();
    const sync = await import("../../lib/webUiSync");
    const intros = await import("../../lib/settingsIntros");
    const { SettingsView: FreshView } = await import("../SettingsView");
    localStorage.setItem(KEY("worktree"), "folded");
    let respond: (state: Record<string, string>) => void = () => {};
    vi.mocked(api.getWebUiState).mockReturnValue(new Promise((resolve) => (respond = resolve)));
    const hydrating = sync.hydrateWebUiStateFromServer();

    intros.resetSettingsIntros(false);
    // The Daemon's older answer arrives after the replay.
    await act(async () => {
      respond({ [KEY("worktree")]: "folded" });
      await hydrating;
    });
    expect(localStorage.getItem(KEY("worktree"))).toBe("unfolded");
    render(
      <FreshView
        onClose={() => {}}
        tab="worktree"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
        readOnly={false}
      />,
    );
    await waitFor(() => expect(intro()).not.toBeNull());
    expect(about()).toBeNull();
  });

  it("gives a replay held for hydration the read-only answer that arrived meanwhile", async () => {
    vi.resetModules();
    const sync = await import("../../lib/webUiSync");
    const intros = await import("../../lib/settingsIntros");
    let respond: (state: Record<string, string>) => void = () => {};
    vi.mocked(api.getWebUiState).mockReturnValue(new Promise((resolve) => (respond = resolve)));
    const hydrating = sync.hydrateWebUiStateFromServer();
    // "Show tutorial" before /api/about answered, then about says read-only before hydration settles.
    intros.resetSettingsIntros(undefined);
    intros.settleSettingsIntrosReadOnly(true);
    await act(async () => {
      respond({ [KEY("worktree")]: "folded" });
      await hydrating;
    });
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBe("unfolded");
    expect(intros.isSettingsIntroFolded("worktree")).toBe(false);
  });

  /** Fresh modules, so nothing earlier in the file has told the intros whether the Daemon is read-only. */
  async function freshIntros() {
    vi.resetModules();
    const sync = await import("../../lib/webUiSync");
    const intros = await import("../../lib/settingsIntros");
    const { SettingsView: FreshView } = await import("../SettingsView");
    vi.mocked(api.getWebUiState).mockResolvedValue(null);
    await sync.hydrateWebUiStateFromServer();
    // App passes `serverAbout?.read_only`, which is undefined until /api/about loads.
    const view = (readOnly?: boolean) => (
      <FreshView
        onClose={() => {}}
        tab="worktree"
        onSelectTab={() => {}}
        onServerAboutRefresh={() => {}}
        readOnly={readOnly}
      />
    );
    return { sync, intros, view };
  }

  it("folds only the synced key while read-only is unknown, so a replay elsewhere still reaches it", async () => {
    const { sync, view } = await freshIntros();
    const { unmount } = render(view());
    await waitFor(() => expect(intro()).not.toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Got it" }));
    unmount();
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "unfolded" });
    await act(() => sync.hydrateWebUiStateFromServer());
    render(view(false));
    await waitFor(() => expect(intro()).not.toBeNull());
    expect(about()).toBeNull();
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBeNull();
  });

  it("moves a fold made before read-only was known onto the device once it resolves to read-only", async () => {
    const { sync, intros, view } = await freshIntros();
    const { rerender } = render(view());
    await waitFor(() => expect(intro()).not.toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Got it" }));
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBeNull();
    // What App does once /api/about answers.
    intros.settleSettingsIntrosReadOnly(true);
    rerender(view(true));
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBe("folded");
    // The Daemon refused the synced write, so hydration brings its own value back.
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "unfolded" });
    await act(() => sync.hydrateWebUiStateFromServer());
    expect(about()).not.toBeNull();
  });

  it("keeps a read-only dashboard's fold on the device, where hydration can't undo it", async () => {
    localStorage.setItem(KEY("worktree"), "unfolded");
    renderTab("worktree", true);
    await waitFor(() => expect(intro()).not.toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Got it" }));
    // The Daemon refuses read-only writes, so the synced key keeps its value.
    expect(localStorage.getItem(KEY("worktree"))).toBe("unfolded");
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBe("folded");

    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "unfolded" });
    await act(() => hydrateWebUiStateFromServer());
    expect(about()).not.toBeNull();

    // A replay is refused too, so it stays on the device and outlasts the Daemon's "folded".
    act(() => resetSettingsIntros(true));
    expect(intro()).not.toBeNull();
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBe("unfolded");
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "folded" });
    await act(() => hydrateWebUiStateFromServer());
    expect(intro()).not.toBeNull();
  });

  it("drops a read-only device override once a writable dashboard folds", async () => {
    localStorage.setItem("aoe-local-settings-intro-worktree", "unfolded");
    renderTab("worktree");
    await waitFor(() => expect(intro()).not.toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Got it" }));
    expect(localStorage.getItem(KEY("worktree"))).toBe("folded");
    expect(localStorage.getItem("aoe-local-settings-intro-worktree")).toBeNull();
  });

  it.each<[string, string, Record<string, string>, "intro" | "about" | "neither"]>([
    ["folded on another Device, then hydrated", "worktree", { worktree: "folded" }, "about"],
    ["unfolded by a replay on another Device", "worktree", { worktree: "unfolded" }, "intro"],
    ["another tab folded", "worktree", { plugins: "folded" }, "intro"],
    ["a tab without an intro", "sandbox", {}, "neither"],
  ])("%s", async (_label, tab, stored, shown) => {
    for (const [id, value] of Object.entries(stored)) localStorage.setItem(KEY(id), value);
    renderTab(tab);
    await waitFor(() => expect(screen.queryByText(/Loading settings/)).toBeNull());
    expect(intro() !== null).toBe(shown === "intro");
    expect(about() !== null).toBe(shown === "about");
  });
});
