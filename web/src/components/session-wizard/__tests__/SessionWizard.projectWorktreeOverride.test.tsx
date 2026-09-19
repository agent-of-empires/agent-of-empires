// @vitest-environment jsdom
//
// The sidebar's "+" quick-create (App.tsx's handleCreateSession) opens the
// wizard with `prefill.path` already set to a known project, never going
// through ProjectStep's own Recent/Browse/Clone selection — the only path
// that previously reported a project's worktree-default override via
// onSelectSavedProject. Without `prefill.worktreeEnabled`, that override was
// silently dropped and the wizard fell back to the global default. Reported
// live: setting a project's override to Off still showed the (conflicting)
// global default of On after clicking "+".

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
  fetchProjects: vi.fn().mockResolvedValue([]),
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
  });

  it("applies the project's override even against a conflicting global default", async () => {
    // The global default (worktree.enabled: true) deliberately conflicts
    // with the project's override (false) — if the reducer ever ignored the
    // override and just applied the profile default, this would still pass
    // by coincidence with matching values.
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
});
