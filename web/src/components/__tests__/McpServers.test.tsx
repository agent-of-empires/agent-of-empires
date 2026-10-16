// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import type { McpMutationResult, McpServerInput, McpServersResponse, McpResolveResult } from "../../lib/api";

const fetchMcpServers = vi.fn<[string?], Promise<McpServersResponse | null>>();
const resolveMcpConflict = vi.fn<[string, string, "aoe" | "native", string], Promise<McpResolveResult>>();
const keepMcpServer = vi.fn<[string, string], Promise<boolean>>();
const dropMcpServer = vi.fn<[string, string], Promise<boolean>>();
const createMcpServer = vi.fn<[string, McpServerInput], Promise<McpMutationResult>>();
const updateMcpServer = vi.fn<[string, McpServerInput], Promise<McpMutationResult>>();
const deleteMcpServer = vi.fn<[string], Promise<McpMutationResult>>();

vi.mock("../../lib/api", () => ({
  fetchMcpServers: (agent?: string) => fetchMcpServers(agent),
  resolveMcpConflict: (name: string, agent: string, winner: "aoe" | "native", fingerprint: string) =>
    resolveMcpConflict(name, agent, winner, fingerprint),
  keepMcpServer: (name: string, agent: string) => keepMcpServer(name, agent),
  dropMcpServer: (name: string, agent: string) => dropMcpServer(name, agent),
  createMcpServer: (name: string, input: McpServerInput) => createMcpServer(name, input),
  updateMcpServer: (name: string, input: McpServerInput) => updateMcpServer(name, input),
  deleteMcpServer: (name: string) => deleteMcpServer(name),
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
  createMcpServer.mockReset();
  updateMcpServer.mockReset();
  deleteMcpServer.mockReset();
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

  it("a failed keep shows a notice and leaves the row in place", async () => {
    fetchMcpServers.mockResolvedValue(keptResponse());
    keepMcpServer.mockResolvedValue(false);
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("keep gone"));
    expect(await screen.findByText(/Could not keep "gone"/)).toBeTruthy();
    expect(screen.getByLabelText("keep gone")).toBeTruthy();
  });
});

describe("McpServers add / edit / delete", () => {
  const OWNED = {
    name: "fs",
    transport: "stdio",
    command: "mcp-fs",
    args: ["--root", "."],
    envNames: ["TOKEN"],
    provenance: "global",
  };
  const NATIVE = { name: "native", transport: "stdio", command: "n", provenance: "agent-native:claude" };

  it("offers edit and delete only on global rows, and nothing when read-only", async () => {
    fetchMcpServers.mockResolvedValue(response({ effective: [OWNED, NATIVE] }));
    const { unmount } = render(<McpServers />);
    await screen.findByLabelText("edit fs");
    expect(screen.getByLabelText("delete fs")).toBeTruthy();
    expect(screen.queryByLabelText("edit native")).toBeNull();
    expect(screen.queryByLabelText("delete native")).toBeNull();
    unmount();

    render(<McpServers readOnly />);
    await screen.findByText("fs");
    expect(screen.queryByText("Add server")).toBeNull();
    expect(screen.queryByLabelText("edit fs")).toBeNull();
  });

  it("adds a stdio server with its env value and reloads", async () => {
    fetchMcpServers.mockResolvedValue(response());
    createMcpServer.mockResolvedValue({ ok: true, status: 201 });
    render(<McpServers />);
    fireEvent.click(await screen.findByText("Add server"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "fs" } });
    fireEvent.change(within(dialog).getByLabelText("Command"), { target: { value: "mcp-fs" } });
    fireEvent.click(within(dialog).getByText("Add argument"));
    fireEvent.change(within(dialog).getByLabelText("argument 1"), { target: { value: "--root" } });
    fireEvent.click(within(dialog).getByText("Add argument"));
    fireEvent.change(within(dialog).getByLabelText("argument 2"), { target: { value: "." } });
    fireEvent.click(within(dialog).getByText("Add env"));
    fireEvent.change(within(dialog).getByLabelText("env name 1"), { target: { value: "TOKEN" } });
    fireEvent.change(within(dialog).getByLabelText("env value 1"), { target: { value: "s3cret" } });
    fireEvent.click(within(dialog).getByText("Save"));
    await waitFor(() =>
      expect(createMcpServer).toHaveBeenCalledWith("fs", {
        transport: "stdio",
        command: "mcp-fs",
        args: ["--root", "."],
        env: [{ name: "TOKEN", value: "s3cret" }],
      }),
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("an untouched stored secret is sent without a value; a typed one replaces it; a removed one is omitted", async () => {
    const twoSecrets = { ...OWNED, envNames: ["KEEP", "SWAP", "DROP"] };
    fetchMcpServers.mockResolvedValue(response({ effective: [twoSecrets] }));
    updateMcpServer.mockResolvedValue({ ok: true, status: 200 });
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("edit fs"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("env value 2"), { target: { value: "new" } });
    fireEvent.click(within(dialog).getByLabelText("remove env 3"));
    fireEvent.click(within(dialog).getByText("Save"));
    await waitFor(() =>
      expect(updateMcpServer).toHaveBeenCalledWith("fs", {
        transport: "stdio",
        command: "mcp-fs",
        args: ["--root", "."],
        env: [{ name: "KEEP" }, { name: "SWAP", value: "new" }],
      }),
    );
  });

  it("saves empty and multiline arguments unchanged", async () => {
    const args = ["--label", "", "--root", ".", "line1\nline2"];
    fetchMcpServers.mockResolvedValue(response({ effective: [{ ...OWNED, args, envNames: [] }] }));
    updateMcpServer.mockResolvedValue({ ok: true, status: 200 });
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("edit fs"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByText("Save"));
    await waitFor(() =>
      expect(updateMcpServer).toHaveBeenCalledWith("fs", {
        transport: "stdio",
        command: "mcp-fs",
        args,
        env: [],
      }),
    );
  });

  it("keeps the form open and shows the server's message when saving fails", async () => {
    fetchMcpServers.mockResolvedValue(response());
    createMcpServer.mockResolvedValue({ ok: false, status: 409, error: "already exists" });
    render(<McpServers />);
    fireEvent.click(await screen.findByText("Add server"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "fs" } });
    fireEvent.click(within(dialog).getByText("Save"));
    expect((await within(dialog).findByRole("alert")).textContent).toBe("already exists");
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("deletes after confirmation and not when declined", async () => {
    fetchMcpServers.mockResolvedValue(response({ effective: [OWNED] }));
    deleteMcpServer.mockResolvedValue({ ok: true, status: 204 });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<McpServers />);
    fireEvent.click(await screen.findByLabelText("delete fs"));
    expect(deleteMcpServer).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    fetchMcpServers.mockResolvedValue(response());
    fireEvent.click(screen.getByLabelText("delete fs"));
    await waitFor(() => expect(deleteMcpServer).toHaveBeenCalledWith("fs"));
    await waitFor(() => expect(screen.queryByLabelText("delete fs")).toBeNull());
    confirm.mockRestore();
  });
});
