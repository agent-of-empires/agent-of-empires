// @vitest-environment jsdom
//
// Wizard remembers the project of the last launched session across opens,
// the same per-browser way it remembers the last tool and instruction.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";

const createSession = vi.fn();
const fetchIsGitRepo = vi.fn().mockResolvedValue(true);
const fetchRecentProjects = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: (...args: unknown[]) => fetchIsGitRepo(...args),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue([]),
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
    fetchIsGitRepo.mockResolvedValue(true);
    fetchRecentProjects.mockResolvedValue(RECENTS);
  });

  it("opens a plain New session on the remembered project, one Launch from a session", async () => {
    // A path no mock lists, so the seed can only have come from storage.
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    const { getByText } = renderWizard();

    await waitFor(() => expect(launchButton(getByText).disabled).toBe(false));
    expect(fetchIsGitRepo).toHaveBeenCalledWith("/tmp/remembered");
    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/tmp/remembered" });
  });

  const prefillCases: Array<{ name: string; stored: string | null; expectedPath: string }> = [
    { name: "a launch writes the path it used", stored: null, expectedPath: "/tmp/other" },
    { name: "a prefill path wins over the memory", stored: "/tmp/remembered", expectedPath: "/tmp/other" },
  ];
  for (const c of prefillCases) {
    it(c.name, async () => {
      if (c.stored) localStorage.setItem(PROJECT_KEY, c.stored);
      const { getByText } = renderWizard({ path: "/tmp/other", tool: "claude" });

      fireEvent.click(getByText(/Launch session/));

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: c.expectedPath });
      await waitFor(() => expect(localStorage.getItem(PROJECT_KEY)).toBe(c.expectedPath));
    });
  }

  it("leaves the memory alone on a scratch launch and does not seed a scratch open", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/remembered");
    const { getByText } = renderWizard({ scratch: true });

    fireEvent.click(getByText(/Launch session/));

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

    fireEvent.click(getByText(/Launch session/));

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
});
