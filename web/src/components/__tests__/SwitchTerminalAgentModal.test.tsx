// @vitest-environment jsdom

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { SwitchTerminalAgentModal } from "../SwitchTerminalAgentModal";
import { fetchAgents, switchTerminalAgent } from "../../lib/api";

vi.mock("../../lib/api", () => ({
  fetchAgents: vi.fn(),
  switchTerminalAgent: vi.fn(),
}));

vi.mock("../../lib/toastBus", () => ({
  reportError: vi.fn(),
  reportInfo: vi.fn(),
}));

const fetchAgentsMock = vi.mocked(fetchAgents);
const switchTerminalAgentMock = vi.mocked(switchTerminalAgent);

describe("SwitchTerminalAgentModal", () => {
  beforeEach(() => {
    fetchAgentsMock.mockResolvedValue([
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
    ]);
    switchTerminalAgentMock.mockResolvedValue({
      session_id: "s-1",
      tool: "codex",
      status: "running",
      context_handoff: "sent",
    });
  });

  it("offers another installed tool and switches the existing session", async () => {
    render(<SwitchTerminalAgentModal open sessionId="s-1" currentTool="claude" onClose={vi.fn()} />);

    await waitFor(() => expect(screen.getByRole("radio", { name: /codex/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(switchTerminalAgentMock).toHaveBeenCalledWith("s-1", "codex"));
  });
});
