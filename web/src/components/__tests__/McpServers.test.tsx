// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

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
  it("each winner button posts its winner with the fingerprint and reloads", async () => {
    for (const [button, winner] of [
      ["Keep AoE version", "aoe"],
      ["Use native", "native"],
    ] as [string, "aoe" | "native"][]) {
      resolveMcpConflict.mockResolvedValue("applied");
      const dialog = await openConflictModal();
      // After an applied resolution the surface reloads with no conflict.
      fetchMcpServers.mockResolvedValue(response());
      fireEvent.click(within(dialog).getByText(button));
      await waitFor(() => expect(resolveMcpConflict).toHaveBeenCalledWith("fs", "claude", winner, "fp-123"));
      await waitFor(() => expect(screen.queryByLabelText("resolve fs")).toBeNull());
      cleanup();
    }
  });

  it("stale and error results show their notices", async () => {
    for (const [result, notice] of [
      ["stale", /already resolved by another surface/],
      ["error", /Could not resolve "fs"/],
    ] as [McpResolveResult, RegExp][]) {
      resolveMcpConflict.mockResolvedValue(result);
      const dialog = await openConflictModal();
      fireEvent.click(within(dialog).getByText("Keep AoE version"));
      expect(await screen.findByText(notice)).toBeTruthy();
      cleanup();
    }
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

  it("keep and drop apply to the server and reload on success", async () => {
    for (const [action, call] of [
      ["keep", keepMcpServer],
      ["drop", dropMcpServer],
    ] as [string, typeof keepMcpServer][]) {
      fetchMcpServers.mockResolvedValue(keptResponse());
      call.mockResolvedValue(true);
      const { unmount } = render(<McpServers />);
      const button = await screen.findByLabelText(`${action} gone`);
      fetchMcpServers.mockResolvedValue(response());
      fireEvent.click(button);
      await waitFor(() => expect(call).toHaveBeenCalledWith("gone", "claude"));
      await waitFor(() => expect(screen.queryByLabelText(`${action} gone`)).toBeNull());
      unmount();
    }
  });

  it("a failed keep shows a notice and leaves the row in place", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    keepMcpServer.mockResolvedValue(false);
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("keep gone"));
    expect(await screen.findByText(/Could not keep "gone"/)).toBeTruthy();
    expect(screen.getByLabelText("keep gone")).toBeTruthy();
  });
});
