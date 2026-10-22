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

beforeEach(() => localStorage.clear());

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
    act(() => resetSettingsIntros());
    expect(intro()).not.toBeNull();
    expect(about()).toBeNull();
    // Written, not removed, so the reset reaches Devices that hydrate from the Daemon.
    for (const id of ["worktree", "plugins", "structured-view"]) {
      expect(localStorage.getItem(KEY(id)), id).toBe("unfolded");
    }
  });

  it("re-reads the folds when the Daemon's web UI state arrives after the view mounted", async () => {
    renderTab("worktree");
    await waitFor(() => expect(intro()).not.toBeNull());
    vi.mocked(api.getWebUiState).mockResolvedValue({ [KEY("worktree")]: "folded" });
    await act(() => hydrateWebUiStateFromServer());
    expect(intro()).toBeNull();
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

    act(() => resetSettingsIntros());
    expect(intro()).not.toBeNull();
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
