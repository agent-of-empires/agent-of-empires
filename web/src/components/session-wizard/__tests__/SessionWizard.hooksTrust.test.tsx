// @vitest-environment jsdom
//
// Wizard wiring for the on_create hooks-trust flow (#2066): a create refused
// with `hooksNeedTrust` pauses on the HooksTrustDialog; Proceed replays the
// same request with `trust_hooks: true`, Cancel returns to the wizard without
// a second submit. The mocked Playwright spec covers the same story against
// the real fetch layer; this exercises the SessionWizard handlers directly.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent, waitFor } from "@testing-library/react";

import { SessionWizard } from "../SessionWizard";

const createSession = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue([]),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  // The single-screen wizard mounts ProjectStep on open (#2210); stub its
  // recent-project fetches. One seeded recent keeps the Recent tab active.
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: vi.fn().mockResolvedValue({
    projects: [{ path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" }],
  }),
  fetchProjects: vi.fn().mockResolvedValue([]),
  createSession: (...args: unknown[]) => createSession(...args),
}));

afterEach(() => {
  cleanup();
});

const HOOKS_REFUSAL = {
  ok: false,
  error: "Repository hooks require trust.",
  hooksNeedTrust: {
    onCreate: ["bash scripts/setup-worktree.sh"],
    onLaunch: ["npm start"],
    onDestroy: [],
    needsMcpTrust: false,
  },
};

function renderWizard(onCreated: (session: unknown) => void = () => {}) {
  return render(
    <SessionWizard onClose={() => {}} onCreated={onCreated} prefill={{ path: "/tmp/proj", tool: "claude" }} />,
  );
}

describe("SessionWizard hooks-trust flow (#2066)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("pauses on the trust dialog, then resubmits with trust_hooks on Proceed", async () => {
    const onCreated = vi.fn();
    createSession.mockResolvedValueOnce(HOOKS_REFUSAL).mockResolvedValueOnce({ ok: true, session: { id: "s1" } });
    const { getByText, getByTestId } = renderWizard(onCreated);

    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(getByTestId("hooks-trust-dialog")).toBeTruthy());
    expect(getByTestId("hooks-trust-list").textContent).toContain("bash scripts/setup-worktree.sh");
    expect(getByTestId("hooks-trust-list").textContent).toContain("npm start");
    expect(createSession).toHaveBeenCalledTimes(1);
    expect(createSession.mock.calls[0][0]).not.toHaveProperty("trust_hooks", true);

    fireEvent.click(getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(createSession.mock.calls[1][0]).toMatchObject({ trust_hooks: true });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("Cancel dismisses the dialog without a second submit", async () => {
    createSession.mockResolvedValue(HOOKS_REFUSAL);
    const { getByText, getByTestId, queryByTestId } = renderWizard();

    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(getByTestId("hooks-trust-dialog")).toBeTruthy());

    fireEvent.click(getByText("Cancel"));
    await waitFor(() => expect(queryByTestId("hooks-trust-dialog")).toBeNull());
    expect(createSession).toHaveBeenCalledTimes(1);
  });

  it("a refusal that already carried trust_hooks falls through to a plain error", async () => {
    // The `!body.trust_hooks` guard: if the server still refuses after the
    // user opted in, surface the error instead of looping the dialog.
    createSession.mockResolvedValue(HOOKS_REFUSAL);
    const { getByText, getByTestId } = renderWizard();

    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(getByTestId("hooks-trust-dialog")).toBeTruthy());
    fireEvent.click(getByTestId("hooks-trust-proceed"));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(getByText("Repository hooks require trust.")).toBeTruthy());
  });
});
