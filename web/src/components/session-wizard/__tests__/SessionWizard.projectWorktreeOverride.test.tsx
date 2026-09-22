// @vitest-environment jsdom
//
// Paths the wizard opens on without a ProjectStep selection: the sidebar's "+" quick-create
// (`prefill.worktreeEnabled`) and the remembered last-used project.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";

async function clickLaunch(getByText: (m: RegExp) => HTMLElement) {
  const button = getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
}

const createSession = vi.fn();
const fetchSettings = vi.fn();
const fetchProjects = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue({ acknowledged: true, globs: [] }),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: vi.fn().mockResolvedValue({ projects: [] }),
  fetchProjects: (...args: unknown[]) => fetchProjects(...args),
  createSession: (...args: unknown[]) => createSession(...args),
}));

function renderWizard(prefill?: WizardPrefill) {
  return render(<SessionWizard onClose={() => {}} onCreated={() => {}} prefill={prefill} />);
}

afterEach(() => {
  cleanup();
  localStorage.clear();
});

describe("SessionWizard prefill.worktreeEnabled (project override on quick-create)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    fetchProjects.mockResolvedValue([]);
  });

  it("applies the project's override even against a conflicting global default", async () => {
    // Conflicting values, so ignoring the override cannot pass by coincidence.
    let resolveSettings!: (settings: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    const { getByText } = renderWizard({ path: "/repo/alpha", worktreeEnabled: false });

    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    await act(async () => resolveSettings({ worktree: { enabled: true } }));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", worktree_enabled: false });
  });

  it("falls back to the global default when the project has no override", async () => {
    let resolveSettings!: (settings: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    const { getByText } = renderWizard({ path: "/repo/beta" });

    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    await act(async () => resolveSettings({ worktree: { enabled: true } }));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/beta", worktree_enabled: true });
  });

  it.each([
    ["settings before projects", true],
    ["projects before settings", false],
  ])("applies the override to a remembered last-used path (%s)", async (_, settingsFirst) => {
    localStorage.setItem("aoe-new-session-last-project", "/repo/alpha");
    let resolveSettings!: (settings: unknown) => void;
    let resolveProjects!: (projects: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    fetchProjects.mockReturnValue(new Promise((resolve) => (resolveProjects = resolve)));
    const { getByText } = renderWizard();

    const projects = [
      { name: "alpha", path: "/repo/alpha", scope: "global", pinned: false, overrides: { worktree_enabled: false } },
    ];
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    await waitFor(() => expect(fetchProjects).toHaveBeenCalled());
    const settings = () => resolveSettings({ worktree: { enabled: true } });
    await act(async () => (settingsFirst ? settings() : resolveProjects(projects)));
    await act(async () => (settingsFirst ? resolveProjects(projects) : settings()));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", worktree_enabled: false });
  });
});
