// @vitest-environment jsdom
//
// Wizard remembers the project of the last launched session across opens,
// the same per-browser way it remembers the last tool and instruction. A
// plain New session opens already pointed at it, so a one-project user is one
// Launch from a session instead of picking the same folder every time. A
// prefill (sidebar +, scratch, clone) still brings its own path or none.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";

const createSession = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue([]),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: vi.fn().mockResolvedValue({
    projects: [{ path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" }],
  }),
  fetchProjects: vi.fn().mockResolvedValue([]),
  createSession: (...args: unknown[]) => createSession(...args),
}));

const PROJECT_KEY = "aoe-new-session-last-project";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderWizard(prefill?: WizardPrefill) {
  return render(<SessionWizard onClose={() => {}} onCreated={() => {}} prefill={prefill} />);
}

function launchButton(getByText: (m: RegExp) => HTMLElement): HTMLButtonElement {
  return getByText(/Launch session/).closest("button") as HTMLButtonElement;
}

describe("SessionWizard last-project memory", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
  });

  it("opens a plain New session on the remembered project, one Launch from a session", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/proj");
    const { getByText } = renderWizard();

    await waitFor(() => expect(launchButton(getByText).disabled).toBe(false));
    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/tmp/proj" });
  });

  it("remembers the path of the session just launched, whichever way it was chosen", async () => {
    const { getByText } = renderWizard({ path: "/tmp/other", tool: "claude" });

    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(localStorage.getItem(PROJECT_KEY)).toBe("/tmp/other"));
  });

  it("lets a prefill path win over the memory", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/proj");
    const { getByText } = renderWizard({ path: "/tmp/other", tool: "claude" });

    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/tmp/other" });
  });

  it("leaves the memory alone on a scratch launch and does not seed a scratch open", async () => {
    localStorage.setItem(PROJECT_KEY, "/tmp/proj");
    const { getByText } = renderWizard({ scratch: true });

    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "" });
    expect(localStorage.getItem(PROJECT_KEY)).toBe("/tmp/proj");
  });

  it("ignores a stored value that is not an absolute path", async () => {
    localStorage.setItem(PROJECT_KEY, "not a path");
    const { getByText } = renderWizard();

    // No project seeded, so nothing to launch yet.
    await waitFor(() => expect(getByText(/Launch session/)).toBeTruthy());
    expect(launchButton(getByText).disabled).toBe(true);
  });
});
