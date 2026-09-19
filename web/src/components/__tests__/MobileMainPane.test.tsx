// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { makeSession as baseSession } from "./fixtures";
import type { useDiffComments } from "../../hooks/useDiffComments";

vi.mock("../TerminalSessionStack", () => ({
  TerminalSessionStack: () => <div data-testid="agent-terminal" />,
}));
vi.mock("../PairedTerminal", () => ({
  PairedShellPane: () => <div data-testid="paired-shell" />,
}));
vi.mock("../diff/DiffFileList", () => ({
  DiffFileList: () => <div data-testid="diff-list" />,
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
  const props: Parameters<typeof MobileMainPane>[0] = {
    view: "agent",
    pluginPanes: [],
    onBackToAgent,
    onOpenAgentsPane: vi.fn(),
    pairedMounted: false,
    activeSession: session(),
    activeSessionId: "s1",
    sessions: [session()],
    serverAbout: null,
    webSettings: { persistentTerminals: false, maxPersistentTerminals: 3 },
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
  return { onBackToAgent };
}

describe("MobileMainPane", () => {
  it("shows the agent terminal and no back header in structured view", () => {
    setup({ view: "agent" });
    expect(screen.getByTestId("agent-terminal")).toBeDefined();
    expect(screen.queryByTestId("mobile-back-to-agent")).toBeNull();
  });

  it("renders the structured view for structured view sessions", async () => {
    setup({ view: "agent", activeSession: session({ view: "structured" }) });
    // StructuredView is lazy-loaded behind Suspense, so await its resolution.
    expect(await screen.findByTestId("acp-view")).toBeDefined();
  });

  it("shows the back header and returns to agent on click", () => {
    const { onBackToAgent } = setup({ view: "paired", pairedMounted: true });
    fireEvent.click(screen.getByTestId("mobile-back-to-agent"));
    expect(onBackToAgent).toHaveBeenCalled();
  });

  it("mounts the paired shell only once activated", () => {
    setup({ view: "agent", pairedMounted: false });
    expect(screen.queryByTestId("paired-shell")).toBeNull();
  });

  it("keeps the paired shell mounted after first activation", () => {
    setup({ view: "agent", pairedMounted: true });
    expect(screen.getByTestId("paired-shell")).toBeDefined();
  });

  it("shows the sub agents panel in agents view", () => {
    setup({ view: "agents", activeSessionId: "s1" });
    expect(screen.getByTestId("background-agents-panel").textContent).toBe("s1");
    expect(screen.getByText("Sub agents")).toBeDefined();
    expect(screen.getByTestId("mobile-back-to-agent")).toBeDefined();
  });

  it("shows the files pane in files view", () => {
    setup({ view: "files", activeSessionId: "s1" });
    expect(screen.getByTestId("files-pane").textContent).toBe("s1");
    expect(screen.getByText("Files")).toBeDefined();
    expect(screen.getByTestId("mobile-back-to-agent")).toBeDefined();
  });

  it("shows the diff file list in diff view", () => {
    setup({ view: "diff" });
    expect(screen.getByTestId("diff-list")).toBeDefined();
    expect(screen.getByText("Diff")).toBeDefined();
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
    expect(screen.getByTestId("mobile-back-to-agent")).toBeDefined();
  });

  it("shows the diff viewer when a file is selected", () => {
    setup({ view: "diff", selectedFilePath: "src/foo.ts" });
    expect(screen.getByTestId("diff-viewer")).toBeDefined();
    expect(screen.queryByTestId("diff-list")).toBeNull();
  });

  it("shows the comments banner when there are comments", () => {
    setup({
      view: "diff",
      commentsEnabled: true,
      diffComments: { ...diffComments, count: 2 } as ReturnType<typeof useDiffComments>,
    });
    expect(screen.getByTestId("comments-banner")).toBeDefined();
  });

  it("renders the send dialog when open", () => {
    setup({
      view: "diff",
      commentsEnabled: true,
      sendDialogOpen: true,
    });
    expect(screen.getByTestId("send-dialog")).toBeDefined();
  });

  it("on send: clears comments + drafts, closes the dialog and the open file", () => {
    const onCloseSendDialog = vi.fn();
    const onClearSelectedFile = vi.fn();
    const store = makeStore({ clearAfterSend: true });
    setup({
      view: "diff",
      commentsEnabled: true,
      sendDialogOpen: true,
      diffComments: store,
      onCloseSendDialog,
      onClearSelectedFile,
    });
    fireEvent.click(screen.getByTestId("send-dialog"));
    expect(store.clearComments).toHaveBeenCalled();
    expect(store.setIntroDraft).toHaveBeenCalledWith("");
    expect(onCloseSendDialog).toHaveBeenCalled();
    expect(onClearSelectedFile).toHaveBeenCalled();
  });

  it("on send with clearAfterSend off: keeps comments but still closes", () => {
    const onCloseSendDialog = vi.fn();
    const store = makeStore({ clearAfterSend: false });
    setup({
      view: "diff",
      commentsEnabled: true,
      sendDialogOpen: true,
      diffComments: store,
      onCloseSendDialog,
    });
    fireEvent.click(screen.getByTestId("send-dialog"));
    expect(store.clearComments).not.toHaveBeenCalled();
    expect(onCloseSendDialog).toHaveBeenCalled();
  });
});
