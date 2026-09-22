// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import type { McpServersResponse, McpResolveResult } from "../../lib/api";

const fetchMcpServers = vi.fn<[string?], Promise<McpServersResponse | null>>();
const resolveMcpConflict = vi.fn<[string, string, "aoe" | "native", string], Promise<McpResolveResult>>();
const keepMcpServer = vi.fn<[string, string], Promise<boolean>>();
const dropMcpServer = vi.fn<[string, string], Promise<boolean>>();

vi.mock("../../lib/api", () => ({
  fetchMcpServers: (agent?: string) => fetchMcpServers(agent),
  resolveMcpConflict: (name: string, agent: string, winner: "aoe" | "native", fingerprint: string) =>
    resolveMcpConflict(name, agent, winner, fingerprint),
  keepMcpServer: (name: string, agent: string) => keepMcpServer(name, agent),
  dropMcpServer: (name: string, agent: string) => dropMcpServer(name, agent),
}));

// Imported after the mock is registered.
import { McpServers } from "../McpServers";

function response(overrides: Partial<McpServersResponse> = {}): McpServersResponse {
  return {
    agent: "claude",
    effective: [],
    keptOnRemoval: [],
    conflicts: [],
    driftPaused: false,
    ...overrides,
  };
}

beforeEach(() => {
  fetchMcpServers.mockReset();
  resolveMcpConflict.mockReset();
  keepMcpServer.mockReset();
  dropMcpServer.mockReset();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("McpServers read view", () => {
  it("shows an error when the surface fails to load", async () => {
    fetchMcpServers.mockResolvedValue(null);
    render(<McpServers />);
    expect(await screen.findByText("Could not load MCP servers")).toBeTruthy();
  });

  it("renders the effective set with provenance, redacted detail, and shadows", async () => {
    fetchMcpServers.mockResolvedValue(
      response({
        effective: [
          {
            name: "fs",
            transport: "stdio",
            command: "mcp-fs",
            args: ["--root", "."],
            envNames: ["TOKEN"],
            provenance: "global",
            shadowed: ["agent-native:claude"],
          },
          {
            name: "remote",
            transport: "http",
            url: "https://example/mcp",
            headerNames: ["Authorization"],
            provenance: "agent-native:claude",
          },
        ],
      }),
    );
    render(<McpServers />);
    const panel = await screen.findByTestId("mcp-panel");
    expect(within(panel).getByText("fs")).toBeTruthy();
    expect(within(panel).getByText("global")).toBeTruthy();
    // Redacted detail: command/args plus the env NAME, never a value.
    expect(panel.textContent).toContain("mcp-fs --root .");
    expect(panel.textContent).toContain("env: TOKEN");
    expect(panel.textContent).toContain("shadows: agent-native:claude");
    // Remote transport renders its url and the header NAME only.
    expect(panel.textContent).toContain("https://example/mcp");
    expect(panel.textContent).toContain("headers: Authorization");
  });

  it("surfaces the drift-paused note", async () => {
    fetchMcpServers.mockResolvedValue(response({ driftPaused: true }));
    render(<McpServers />);
    expect(await screen.findByText(/Drift detection is paused/)).toBeTruthy();
  });
});

const CONFLICT = {
  name: "fs",
  agent: "claude",
  previous: "fs (stdio): old",
  current: "fs (stdio): new",
  fingerprint: "fp-123",
};

async function openConflictModal() {
  fetchMcpServers.mockResolvedValue(response({ conflicts: [CONFLICT] }));
  render(<McpServers />);
  const resolveBtn = await screen.findByLabelText("resolve fs");
  fireEvent.click(resolveBtn);
  return screen.findByRole("dialog");
}

describe("McpServers conflict resolution", () => {
  it.each([
    ["Keep AoE version", "aoe"],
    ["Use native", "native"],
  ] as [string, "aoe" | "native"][])(
    "'%s' posts its winner with the fingerprint and reloads",
    async (button, winner) => {
      resolveMcpConflict.mockResolvedValue("applied");
      const dialog = await openConflictModal();
      // After an applied resolution the surface reloads with no conflict.
      fetchMcpServers.mockResolvedValue(response());
      fireEvent.click(within(dialog).getByText(button));
      await waitFor(() => expect(resolveMcpConflict).toHaveBeenCalledWith("fs", "claude", winner, "fp-123"));
      await waitFor(() => expect(screen.queryByLabelText("resolve fs")).toBeNull());
    },
  );

  it.each([
    ["stale", /already resolved by another surface/],
    ["error", /Could not resolve "fs"/],
  ] as [McpResolveResult, RegExp][])("a %s result shows its notice", async (result, notice) => {
    resolveMcpConflict.mockResolvedValue(result);
    const dialog = await openConflictModal();
    fireEvent.click(within(dialog).getByText("Keep AoE version"));
    expect(await screen.findByText(notice)).toBeTruthy();
  });

  it("cancel closes the modal without resolving", async () => {
    const dialog = await openConflictModal();
    fireEvent.click(within(dialog).getByText("Cancel"));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(resolveMcpConflict).not.toHaveBeenCalled();
  });
});

describe("McpServers keep / drop", () => {
  function keptResponse() {
    return response({
      keptOnRemoval: [
        {
          name: "gone",
          transport: "stdio",
          command: "g",
          provenance: "kept-on-removal:claude",
        },
      ],
    });
  }

  it.each([
    ["keep", keepMcpServer],
    ["drop", dropMcpServer],
  ] as [string, typeof keepMcpServer][])("%s applies to the server and reloads on success", async (action, call) => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    call.mockResolvedValue(true);
    render(<McpServers />);
    const button = await screen.findByLabelText(`${action} gone`);
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(button);
    await waitFor(() => expect(call).toHaveBeenCalledWith("gone", "claude"));
    await waitFor(() => expect(screen.queryByLabelText(`${action} gone`)).toBeNull());
  });

  it.each([
    ["keep", keepMcpServer, /Could not keep "gone"/],
    ["drop", dropMcpServer, /Could not drop "gone"/],
  ] as [string, typeof keepMcpServer, RegExp][])(
    "%s failure shows a notice and leaves the row in place",
    async (action, call, notice) => {
      fetchMcpServers.mockResolvedValue(keptResponse());
      call.mockResolvedValue(false);
      render(<McpServers />);
      fireEvent.click(await screen.findByLabelText(`${action} gone`));
      expect(await screen.findByText(notice)).toBeTruthy();
      expect(screen.getByLabelText(`${action} gone`)).toBeTruthy();
    },
  );
});
