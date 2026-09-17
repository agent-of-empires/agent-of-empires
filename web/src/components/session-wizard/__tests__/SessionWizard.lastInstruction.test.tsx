// @vitest-environment jsdom
//
// Wizard remembers the last agent-instruction across opens (#2614), the same
// per-browser way it already remembers the last tool. A submitted instruction
// is written to localStorage and prefilled into the next wizard open, so a
// user who reuses the same instruction on every session stops retyping it.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SessionWizard } from "../SessionWizard";

/** Launch is gated until the wizard's profile defaults have settled, the
 *  way a real click is; wait for it the same way. */
async function clickLaunch(getByText: (m: RegExp) => HTMLElement) {
  const button = getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
}

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

const INSTRUCTION_KEY = "aoe-new-session-last-instruction";
const MORE_OPTIONS_KEY = "aoe-new-session-more-options-open";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderWizard(onCreated: (session: unknown) => void = () => {}) {
  return render(
    <SessionWizard onClose={() => {}} onCreated={onCreated} prefill={{ path: "/tmp/proj", tool: "claude" }} />,
  );
}

describe("SessionWizard last-instruction memory (#2614)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
  });

  it("prefills the stored instruction into the create payload", async () => {
    localStorage.setItem(INSTRUCTION_KEY, "always be terse");
    const onCreated = vi.fn();
    const { getByText } = renderWizard(onCreated);

    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ custom_instruction: "always be terse" });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("writes the submitted instruction back to localStorage", async () => {
    // Fold open so AgentOptions (and the instruction textarea) render.
    localStorage.setItem(MORE_OPTIONS_KEY, "true");
    const { getByText, getByPlaceholderText } = renderWizard();

    fireEvent.change(getByPlaceholderText("Custom instructions for this session..."), {
      target: { value: "review for security" },
    });
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ custom_instruction: "review for security" });
    await waitFor(() => expect(localStorage.getItem(INSTRUCTION_KEY)).toBe("review for security"));
  });

  it("clears the stored instruction when submitted empty", async () => {
    localStorage.setItem(INSTRUCTION_KEY, "stale text");
    localStorage.setItem(MORE_OPTIONS_KEY, "true");
    const { getByText, getByPlaceholderText } = renderWizard();

    fireEvent.change(getByPlaceholderText("Custom instructions for this session..."), {
      target: { value: "" },
    });
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(localStorage.getItem(INSTRUCTION_KEY)).toBe(""));
  });
});
