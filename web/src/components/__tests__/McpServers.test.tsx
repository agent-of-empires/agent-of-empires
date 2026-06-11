// @vitest-environment jsdom
//
// Contract test for the MCP servers settings panel (#1996). The live
// Playwright spec covers the read/provenance/redaction happy path against a
// real backend; this locks in the mutation handlers (conflict resolve, keep,
// drop) and their notice branches, which the live coverage does not feed into
// the Vitest patch lane. The api module is mocked so each handler's
// success / stale / failure path is driven deterministically.

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
  it("resolving 'Keep AoE version' posts the winner + fingerprint and reloads", async () => {
    resolveMcpConflict.mockResolvedValue("applied");
    const dialog = await openConflictModal();
    // After an applied resolution the surface reloads with no conflict.
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(within(dialog).getByText("Keep AoE version"));
    await waitFor(() => expect(resolveMcpConflict).toHaveBeenCalledWith("fs", "claude", "aoe", "fp-123"));
    await waitFor(() => expect(screen.queryByLabelText("resolve fs")).toBeNull());
  });

  it("'Use native' resolves with the native winner", async () => {
    resolveMcpConflict.mockResolvedValue("applied");
    const dialog = await openConflictModal();
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(within(dialog).getByText("Use native"));
    await waitFor(() => expect(resolveMcpConflict).toHaveBeenCalledWith("fs", "claude", "native", "fp-123"));
  });

  it("a stale result shows the 'already resolved' notice", async () => {
    resolveMcpConflict.mockResolvedValue("stale");
    const dialog = await openConflictModal();
    fireEvent.click(within(dialog).getByText("Keep AoE version"));
    expect(await screen.findByText(/already resolved by another surface/)).toBeTruthy();
  });

  it("an error result shows the failure notice", async () => {
    resolveMcpConflict.mockResolvedValue("error");
    const dialog = await openConflictModal();
    fireEvent.click(within(dialog).getByText("Keep AoE version"));
    expect(await screen.findByText(/Could not resolve "fs"/)).toBeTruthy();
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

  it("keep promotes the server and reloads on success", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    keepMcpServer.mockResolvedValue(true);
    render(<McpServers />);
    const keepBtn = await screen.findByLabelText("keep gone");
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(keepBtn);
    await waitFor(() => expect(keepMcpServer).toHaveBeenCalledWith("gone", "claude"));
    await waitFor(() => expect(screen.queryByLabelText("keep gone")).toBeNull());
  });

  it("keep failure shows a notice and does not clear the row", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    keepMcpServer.mockResolvedValue(false);
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("keep gone"));
    expect(await screen.findByText(/Could not keep "gone"/)).toBeTruthy();
    expect(screen.getByLabelText("keep gone")).toBeTruthy();
  });

  it("drop discards the server on success", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    dropMcpServer.mockResolvedValue(true);
    render(<McpServers />);
    const dropBtn = await screen.findByLabelText("drop gone");
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(dropBtn);
    await waitFor(() => expect(dropMcpServer).toHaveBeenCalledWith("gone", "claude"));
  });

  it("drop failure shows a notice", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    dropMcpServer.mockResolvedValue(false);
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("drop gone"));
    expect(await screen.findByText(/Could not drop "gone"/)).toBeTruthy();
  });
});
