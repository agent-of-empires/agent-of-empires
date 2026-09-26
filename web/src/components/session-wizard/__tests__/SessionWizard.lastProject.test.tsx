// @vitest-environment jsdom
//
// Wizard remembers the project of the last launched session across opens,
// the same per-browser way it remembers the last tool and instruction.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";

/** Launch is gated until the wizard's profile defaults have settled, the
 *  way a real click is; wait for it the same way. */
async function clickLaunch(getByText: (m: RegExp) => HTMLElement) {
  const button = getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
}

const createSession = vi.fn();
const fetchSettings = vi.fn();
const fetchProfiles = vi.fn();
const fetchIsGitRepo = vi.fn().mockResolvedValue(true);
const fetchRecentProjects = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: (...args: unknown[]) => fetchIsGitRepo(...args),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: (...args: unknown[]) => fetchProfiles(...args),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue({ acknowledged: true, globs: [] }),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: (...args: unknown[]) => fetchRecentProjects(...args),
  fetchProjects: vi.fn().mockResolvedValue([]),
  createSession: (...args: unknown[]) => createSession(...args),
}));

const PROJECT_KEY = "aoe-new-session-last-project";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

const RECENTS = {
  projects: [{ path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" }],
};

function renderWizard(prefill?: WizardPrefill, nameOnly = false) {
  return render(<SessionWizard onClose={() => {}} onCreated={() => {}} prefill={prefill} nameOnly={nameOnly} />);
}

function launchButton(getByText: (m: RegExp) => HTMLElement): HTMLButtonElement {
  return getByText(/Launch session/).closest("button") as HTMLButtonElement;
}

describe("SessionWizard last-project memory", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    fetchSettings.mockResolvedValue({});
    fetchProfiles.mockResolvedValue([]);
    fetchIsGitRepo.mockResolvedValue(true);
    fetchRecentProjects.mockResolvedValue(RECENTS);
  });

  const prefillCases: Array<{ name: string; stored: string | null; expectedPath: string }> = [
    { name: "a launch writes the path it used", stored: null, expectedPath: "/tmp/other" },
    { name: "a prefill path wins over the memory", stored: "/tmp/remembered", expectedPath: "/tmp/other" },
  ];
  for (const c of prefillCases) {
    it(c.name, async () => {
      if (c.stored) localStorage.setItem(PROJECT_KEY, c.stored);
      const { getByText } = renderWizard({ path: "/tmp/other", tool: "claude" });

      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: c.expectedPath });
      await waitFor(() => expect(localStorage.getItem(PROJECT_KEY)).toBe(c.expectedPath));
    });
  }

  it("leaves the memory alone on a scratch launch and does not seed a scratch open", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    const { getByText } = renderWizard({ scratch: true });

    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "" });
    expect(localStorage.getItem(PROJECT_KEY)).toBe("/tmp/remembered");
  });

  it("ignores a stored value that is not an absolute path", async () => {
    localStorage.setItem(PROJECT_KEY, "not a path");
    const { getByText } = renderWizard();

    await waitFor(() => expect(fetchRecentProjects).toHaveBeenCalled());
    expect(fetchIsGitRepo).not.toHaveBeenCalled();
    expect(launchButton(getByText).disabled).toBe(true);
  });

  it("never seeds the hidden path of a name-only (CityHall) wizard", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    const { getByText } = renderWizard(undefined, true);

    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "" });
    expect(fetchIsGitRepo).not.toHaveBeenCalled();
  });

  it("shows the remembered selection even when the picker has no saved or recent rows", async () => {
    // With nothing to pick the step would default to Browse, where the
    // selected-path box is hidden and Launch would be enabled with no target
    // on screen. A remembered path keeps the Recent tab and its box.
    fetchRecentProjects.mockResolvedValue({ projects: [] });
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    const { getByText, findByText } = renderWizard();

    await waitFor(() => expect(launchButton(getByText).disabled).toBe(false));
    expect(await findByText("/tmp/remembered")).toBeTruthy();
  });

  it("holds Launch until the profile defaults have landed, then submits them", async () => {
    // A remembered path satisfies the submit gate at mount, before the chained
    // profiles/settings fetch resolves; a launch in that window would send
    // initialData's sandbox/worktree/yolo instead of the profile's. Reported
    // by review; reproduction adapted from it.
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    let resolveSettings!: (settings: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    const { getByText } = renderWizard();

    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    expect(launchButton(getByText).disabled).toBe(true);
    fireEvent.click(getByText(/Launch session/));
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(createSession).not.toHaveBeenCalled();

    await act(async () => resolveSettings({ sandbox: { enabled_by_default: true }, worktree: { enabled: true } }));
    await waitFor(() => expect(launchButton(getByText).disabled).toBe(false));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({
      path: "/tmp/remembered",
      sandbox: true,
      worktree_enabled: true,
    });
  });

  it("still fetches and applies settings when the profiles fetch fails", async () => {
    // The chain is profiles then settings; a rejected profiles request must
    // fall back to the unresolved global settings, not skip them and launch
    // on initialData. Prove the profile's sandbox default still lands.
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    fetchProfiles.mockRejectedValue(new Error("profiles down"));
    fetchSettings.mockResolvedValue({ sandbox: { enabled_by_default: true } });
    const { getByText } = renderWizard();

    await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith(undefined));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/tmp/remembered", sandbox: true });
  });

  it("does not stay disabled when the settings fetch fails", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    fetchSettings.mockRejectedValue(new Error("boom"));
    const { getByText } = renderWizard();

    await waitFor(() => expect(launchButton(getByText).disabled).toBe(false));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/tmp/remembered" });
  });
});
