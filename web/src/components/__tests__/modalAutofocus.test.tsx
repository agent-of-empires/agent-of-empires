// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type * as Api from "../../lib/api";
import type * as Platform from "../../lib/platform";
import { McpServerForm } from "../McpServerForm";
import { ProjectFormModal } from "../ProjectFormModal";
import { SendCommentsDialog } from "../diff/comments/SendCommentsDialog";

const finePointer = vi.hoisted(() => ({ value: true }));

vi.mock("../../lib/platform", async (importOriginal) => ({
  ...(await importOriginal<typeof Platform>()),
  hasFinePointer: () => finePointer.value,
}));

vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof Api>()),
  getSettingsSchema: vi.fn().mockResolvedValue([]),
  fetchMachineSettings: vi.fn().mockResolvedValue(null),
  resolvePluginOptions: vi.fn().mockResolvedValue([]),
}));

const noop = () => {};
const server = { name: "github", transport: "stdio", command: "gh-mcp", provenance: "global" };
const project = { name: "extra", path: "/repo/extra", scope: "profile" as const, pinned: false };

function sendDialog() {
  return (
    <SendCommentsDialog
      sessionId="s1"
      delivery="structured"
      comments={[]}
      isMultiRepo={false}
      sendEnabled
      sendDisabledReason=""
      introDraft=""
      outroDraft=""
      clearAfterSend={false}
      onChangeIntro={noop}
      onChangeOutro={noop}
      onChangeClearAfterSend={noop}
      onClose={noop}
      onSent={noop}
    />
  );
}

beforeEach(() => {
  finePointer.value = true;
});

afterEach(cleanup);

describe("modals focus their first editable field", () => {
  it("adding an MCP server focuses Name", () => {
    render(<McpServerForm busy={false} error={null} onSubmit={noop} onClose={noop} />);
    expect(document.activeElement).toBe(screen.getByLabelText("Name"));
  });

  it("editing an MCP server focuses Transport, since Name is fixed", () => {
    render(<McpServerForm initial={server} busy={false} error={null} onSubmit={noop} onClose={noop} />);
    expect(document.activeElement).toBe(screen.getByLabelText("Transport"));
  });

  it("adding a project focuses Path", () => {
    render(<ProjectFormModal profile="alpha" onClose={noop} onSaved={async () => {}} />);
    expect(document.activeElement).toBe(screen.getByPlaceholderText("/path/to/repo"));
  });

  it("editing a project focuses the base branch, since Path and Name are fixed", () => {
    render(<ProjectFormModal profile="alpha" initial={project} onClose={noop} onSaved={async () => {}} />);
    expect(document.activeElement).toBe(
      screen.getByPlaceholderText("blank = inherit global default, then auto-detect"),
    );
  });

  it("sending diff comments focuses the intro", () => {
    render(sendDialog());
    expect(document.activeElement).toBe(screen.getByPlaceholderText("Anything you want to say before the comments..."));
  });

  it("a touch device focuses nothing, so the keyboard stays down", () => {
    finePointer.value = false;
    render(sendDialog());
    expect(document.activeElement).toBe(document.body);
  });
});
