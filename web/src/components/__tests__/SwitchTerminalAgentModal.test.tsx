// @vitest-environment jsdom
//
// Modal-side contract for the terminal-agent switch. The modal loads the agent
// catalogue, offers only the entries this machine can launch (never the CLI the
// session is already running), and hands the session to the session-level switch
// endpoint on confirm. The toast reports whether the outgoing terminal context
// could be carried over, and a failed switch is an error, never a silent success.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { fetchAgents, switchTerminalAgent } from "../../lib/api";
import { toastBus } from "../../lib/toastBus";
import { SwitchTerminalAgentModal } from "../SwitchTerminalAgentModal";

vi.mock("../../lib/api", () => ({
  fetchAgents: vi.fn(),
  switchTerminalAgent: vi.fn(),
}));

const mockFetchAgents = vi.mocked(fetchAgents);
const mockSwitch = vi.mocked(switchTerminalAgent);

beforeEach(() => {
  vi.clearAllMocks();
  mockFetchAgents.mockResolvedValue([
    {
      name: "claude",
      kind: "builtin",
      binary: "claude",
      host_only: false,
      installed: true,
      install_hint: "",
      acp_capable: true,
      acp_installed: true,
    },
    {
      name: "codex",
      kind: "builtin",
      binary: "codex",
      host_only: false,
      installed: true,
      install_hint: "",
      acp_capable: true,
      acp_installed: true,
    },
    {
      name: "gemini",
      kind: "builtin",
      binary: "gemini",
      host_only: false,
      installed: false,
      install_hint: "npm i -g @google/gemini-cli",
      acp_capable: true,
      acp_installed: false,
    },
    {
      name: "house-agent",
      kind: "custom",
      binary: "house-agent",
      host_only: false,
      installed: false,
      install_hint: "",
      acp_capable: false,
      acp_installed: false,
    },
  ]);
  mockSwitch.mockResolvedValue({
    ok: true,
    result: { session_id: "s-1", tool: "codex", status: "running", context_handoff: "sent" },
  });
});

afterEach(() => {
  cleanup();
  toastBus.handler = null;
});

function stubToasts() {
  const info = vi.fn();
  const error = vi.fn();
  toastBus.handler = { push: vi.fn(), info, error, openLink: vi.fn() };
  return { info, error };
}

function mount(props?: Partial<React.ComponentProps<typeof SwitchTerminalAgentModal>>) {
  const onClose = vi.fn();
  const utils = render(
    <SwitchTerminalAgentModal open sessionId="s-1" currentTool="claude" onClose={onClose} {...props} />,
  );
  return { onClose, ...utils };
}

describe("SwitchTerminalAgentModal", () => {
  it("offers another installed tool and switches the existing session", async () => {
    const { onClose } = mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(mockSwitch).toHaveBeenCalledWith("s-1", "codex"));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("hides the running tool and uninstalled builtins, keeping custom entries", async () => {
    mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    expect(screen.queryByRole("radio", { name: /claude/ })).toBeNull();
    expect(screen.queryByRole("radio", { name: /gemini/ })).toBeNull();
    expect(screen.getByRole("radio", { name: /house-agent/ })).toBeTruthy();
  });

  it("says so and disables confirm when there is no alternative", async () => {
    mockFetchAgents.mockResolvedValue([
      {
        name: "claude",
        kind: "builtin",
        binary: "claude",
        host_only: false,
        installed: true,
        install_hint: "",
        acp_capable: true,
        acp_installed: true,
      },
    ]);
    mount();

    await waitFor(() => expect(screen.getByText(/No other installed agent was found/)).toBeTruthy());
    expect(screen.getByRole("button", { name: "Switch agent" }).hasAttribute("disabled")).toBe(true);
    expect(mockSwitch).not.toHaveBeenCalled();
  });

  it("reports a refusal that changed nothing as an error and keeps the dialog open", async () => {
    mockSwitch.mockResolvedValue({ ok: false, committed: false, message: "session is already using codex" });
    const { info, error } = stubToasts();
    const { onClose } = mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(error).toHaveBeenCalledWith("session is already using codex"));
    expect(info).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("reports the context as unavailable when the outgoing capture failed", async () => {
    mockSwitch.mockResolvedValue({
      ok: true,
      result: { session_id: "s-1", tool: "codex", status: "running", context_handoff: "unavailable" },
    });
    const { info, error } = stubToasts();
    mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(info).toHaveBeenCalledWith(expect.stringContaining("unavailable")));
    expect(error).not.toHaveBeenCalled();
  });

  it("surfaces a post-swap resume failure and closes, since the row already runs the new tool", async () => {
    mockSwitch.mockResolvedValue({
      ok: false,
      committed: true,
      message: "Resume failed for sid s-9; preserved for explicit retry",
    });
    const { info, error } = stubToasts();
    const { onClose } = mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(error).toHaveBeenCalledWith("Resume failed for sid s-9; preserved for explicit retry"));
    expect(info).not.toHaveBeenCalled();
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("cancel and Escape close without switching", async () => {
    const { onClose } = mount();

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(1);

    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(2);
    expect(mockSwitch).not.toHaveBeenCalled();
  });
});
