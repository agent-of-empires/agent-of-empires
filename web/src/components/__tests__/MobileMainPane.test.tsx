// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { makeSession as baseSession } from "./fixtures";
import type { useDiffComments } from "../../hooks/useDiffComments";

vi.mock("../TerminalSessionStack", () => ({
  TerminalSessionStack: () => <div data-testid="agent-terminal" />,
}));
vi.mock("../PairedTerminal", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../PairedTerminal")>()),
  PairedShellPane: () => <div data-testid="paired-shell" />,
}));
vi.mock("../diff/DiffFileList", () => ({
  DiffFileList: () => <div data-testid="diff-list" />,
}));
vi.mock("../diff/FileContentViewer", () => ({
  FileContentViewer: ({ filePath }: { filePath: string }) => <div data-testid="session-file-viewer">{filePath}</div>,
}));
vi.mock("../diff/DiffFileViewer", () => ({
  DiffFileViewer: () => <div data-testid="diff-viewer" />,
}));
vi.mock("../diff/comments/CommentsBanner", () => ({
  CommentsBanner: () => <div data-testid="comments-banner" />,
}));
vi.mock("../diff/comments/SendCommentsDialog", () => ({
  SendCommentsDialog: ({ onSent }: { onSent: () => void }) => (
    <button data-testid="send-dialog" onClick={onSent}>
      send
    </button>
  ),
}));
vi.mock("../acp/StructuredView", () => ({
  StructuredView: () => <div data-testid="acp-view" />,
}));
vi.mock("../acp/BackgroundAgentsPanel", () => ({
  BackgroundAgentsPanel: ({ sessionId }: { sessionId: string | null }) => (
    <div data-testid="background-agents-panel">{sessionId}</div>
  ),
}));
vi.mock("../FilesPane", () => ({
  FilesPane: ({ sessionId }: { sessionId: string | null }) => <div data-testid="files-pane">{sessionId}</div>,
}));

import { MobileMainPane } from "../MobileMainPane";

const session = (overrides: Partial<SessionResponse> = {}) =>
  baseSession({ id: "s1", title: "t", project_path: "/tmp/t", status: "Running", ...overrides });

function makeStore(overrides: Partial<ReturnType<typeof useDiffComments>> = {}): ReturnType<typeof useDiffComments> {
  return {
    count: 0,
    comments: [],
    introDraft: "",
    outroDraft: "",
    clearAfterSend: true,
    setIntroDraft: vi.fn(),
    setOutroDraft: vi.fn(),
    setClearAfterSend: vi.fn(),
    clearComments: vi.fn(),
    ...overrides,
  } as unknown as ReturnType<typeof useDiffComments>;
}

const diffComments = makeStore();

function setup(overrides: Partial<Parameters<typeof MobileMainPane>[0]> = {}) {
  const onBackToAgent = vi.fn();
  const onPick = vi.fn();
  const onMore = vi.fn();
  const props: Parameters<typeof MobileMainPane>[0] = {
    view: "agent",
    readOnly: false,
    pluginPanes: [],
    onBackToAgent,
    tabs: { available: ["diff", "files", "agents"], terminal: true, onPick, onMore },
    onOpenAgentsPane: vi.fn(),
    pairedMounted: false,
    pairedShell: { session: session(), index: 0 },
    shellMode: "host",
    onShellModeChange: vi.fn(),
    activeSession: session(),
    activeSessionId: "s1",
    sessions: [session()],
    serverAbout: null,
    webSettings: {
      persistentTerminals: false,
      maxPersistentTerminals: 3,
      persistentStructuredViews: false,
      maxPersistentStructuredViews: 3,
    },
    selectedFilePath: null,
    selectedRepoName: undefined,
    revision: 0,
    diffFiles: [],
    perRepoBases: [],
    warning: null,
    diffFilesLoading: false,
    onSelectFile: vi.fn(),
    onCloseFile: vi.fn(),
    onDiffRefresh: vi.fn(),
    commentsEnabled: false,
    commentSendEnabled: false,
    commentSendDisabledReason: undefined,
    diffComments,
    commentsIsMultiRepo: false,
    sendDialogOpen: false,
    onOpenSendDialog: vi.fn(),
    onCloseSendDialog: vi.fn(),
    onClearSelectedFile: vi.fn(),
    ...overrides,
  };
  render(<MobileMainPane {...props} />);
  return { onBackToAgent, onPick, onMore };
}

describe("MobileMainPane", () => {
  it("renders the structured view for structured view sessions", async () => {
    const structured = session({ view: "structured" });
    setup({ view: "agent", activeSession: structured, sessions: [structured] });
    // StructuredView is lazy-loaded behind Suspense, so await its resolution.
    expect(await screen.findByTestId("acp-view")).toBeDefined();
  });

  it("switches views from the tab bar, marking the shown one, with the rest under More", () => {
    const { onPick, onMore } = setup({ view: "paired", pairedMounted: true });
    expect(screen.getByTestId("mobile-tab-paired").getAttribute("aria-current")).toBe("page");
    fireEvent.click(screen.getByTestId("mobile-tab-agent"));
    fireEvent.click(screen.getByTestId("mobile-tab-diff"));
    fireEvent.click(screen.getByTestId("mobile-tab-more"));
    expect([onPick.mock.calls, onMore.mock.calls.length]).toEqual([[["agent"], ["diff"]], 1]);
    cleanup();
    // Diff, Files and Terminal show only where the Session offers them.
    setup({ tabs: { available: [], terminal: false, onPick: vi.fn(), onMore: vi.fn() } });
    expect(
      ["agent", "paired", "diff", "files", "more"].map((t) => screen.queryByTestId(`mobile-tab-${t}`) !== null),
    ).toEqual([true, false, false, false, true]);
  });

  it("names the shown shell, another Session's included, and offers its mode only when sandboxed", () => {
    const onShellModeChange = vi.fn();
    const other = session({ id: "s2", title: "Other", is_sandboxed: true });
    setup({ view: "paired", pairedMounted: true, pairedShell: { session: other, index: 2 }, onShellModeChange });
    expect(screen.getByText("Other · 3")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Container" }));
    expect(onShellModeChange).toHaveBeenCalledWith("container");
    cleanup();
    setup({ view: "paired", pairedMounted: true });
    expect(screen.queryByRole("button", { name: "Container" })).toBeNull();
  });

  it("mounts the paired shell only once activated", () => {
    setup({ view: "agent", pairedMounted: false });
    expect(screen.getByTestId("agent-terminal")).toBeDefined();
    expect(screen.queryByTestId("paired-shell")).toBeNull();
    cleanup();
    setup({ view: "agent", pairedMounted: true });
    expect(screen.getByTestId("paired-shell")).toBeDefined();
  });

  it("passes the active session to the agents and files panes", async () => {
    for (const [view, testId, current] of [
      ["agents", "background-agents-panel", "mobile-tab-more"],
      ["files", "files-pane", "mobile-tab-files"],
    ] as const) {
      setup({ view, activeSessionId: "s1" });
      expect((await screen.findByTestId(testId)).textContent).toBe("s1");
      expect(screen.getByTestId(current).getAttribute("aria-current")).toBe("page");
      cleanup();
    }
  });

  it("uses the provenance-confined viewer for transcript files outside a repository", () => {
    setup({ view: "diff", selectedFilePath: "/tmp/plan.md", selectedFileExternal: true });
    expect(screen.getByTestId("session-file-viewer").textContent).toBe("/tmp/plan.md");
    expect(screen.queryByTestId("diff-viewer")).toBeNull();
    cleanup();
  });

  it("shows the diff list, or the viewer when a file is selected", async () => {
    setup({ view: "diff" });
    expect(screen.getByTestId("diff-list")).toBeDefined();
    cleanup();
    setup({ view: "diff", selectedFilePath: "src/foo.ts" });
    expect(await screen.findByTestId("diff-viewer")).toBeDefined();
    expect(screen.queryByTestId("diff-list")).toBeNull();
  });

  it("renders the plugin pane body and its title for a plugin view", () => {
    const pane = {
      id: "plugin:acme.kit:gh" as const,
      title: "GitHub",
      defaultDock: "right" as const,
      icon: undefined,
      entry: {
        plugin_id: "acme.kit",
        slot: "pane" as const,
        id: "gh",
        session_id: "s1",
        payload: { title: "GitHub", body: "PR #1 open" },
      },
    };
    setup({ view: pane.id, pluginPanes: [pane] });
    expect(screen.getByTestId("plugin-pane-body")).toBeDefined();
    expect(screen.getByText("PR #1 open")).toBeDefined();
    // A view from More names itself on the More tab.
    expect(screen.getByTestId("mobile-tab-more").textContent).toBe("GitHub");
  });

  it("shows the comments banner when there are comments", () => {
    setup({
      view: "diff",
      commentsEnabled: true,
      diffComments: { ...diffComments, count: 2 } as ReturnType<typeof useDiffComments>,
    });
    expect(screen.getByTestId("comments-banner")).toBeDefined();
  });

  it("renders the send dialog when open", async () => {
    setup({
      view: "diff",
      commentsEnabled: true,
      sendDialogOpen: true,
    });
    expect(await screen.findByTestId("send-dialog")).toBeDefined();
  });

  it("on send clears comments + drafts, closes dialog and file, and returns only terminal sessions to agent", async () => {
    for (const [view, returnsToAgent] of [
      ["structured", false],
      ["terminal", true],
    ] as const) {
      const onCloseSendDialog = vi.fn();
      const onClearSelectedFile = vi.fn();
      const store = makeStore({ clearAfterSend: true });
      const { onBackToAgent } = setup({
        activeSession: session({ view }),
        view: "diff",
        commentsEnabled: true,
        sendDialogOpen: true,
        diffComments: store,
        onCloseSendDialog,
        onClearSelectedFile,
      });
      fireEvent.click(await screen.findByTestId("send-dialog"));
      expect(store.clearComments).toHaveBeenCalled();
      expect(store.setIntroDraft).toHaveBeenCalledWith("");
      expect(onCloseSendDialog).toHaveBeenCalled();
      expect(onClearSelectedFile).toHaveBeenCalled();
      expect(onBackToAgent).toHaveBeenCalledTimes(returnsToAgent ? 1 : 0);
      cleanup();
    }
  });

  it("keeps comments when clearAfterSend is off while closing the dialog", async () => {
    const onCloseSendDialog = vi.fn();
    const store = makeStore({ clearAfterSend: false });
    setup({
      view: "diff",
      commentsEnabled: true,
      sendDialogOpen: true,
      diffComments: store,
      onCloseSendDialog,
    });
    fireEvent.click(await screen.findByTestId("send-dialog"));
    expect(store.clearComments).not.toHaveBeenCalled();
    expect(onCloseSendDialog).toHaveBeenCalled();
  });
});
