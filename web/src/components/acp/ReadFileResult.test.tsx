// @vitest-environment jsdom
// Shiki is mocked so HighlightedBlock renders a plain <pre> synchronously.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue(null),
  getSnippetHighlighter: vi.fn().mockResolvedValue(null),
  langHintForPath: () => "",
}));
vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));
vi.mock("../../hooks/useArtifactObjectUrl", () => ({
  useArtifactObjectUrl: (url: string) => `blob:${url}`,
}));
vi.mock("../../lib/openInNewTab", () => ({ openInNewTab: vi.fn().mockResolvedValue({ ok: true }) }));
vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/api")>()),
  getSessionFile: vi.fn(),
}));

import { getSessionFile } from "../../lib/api";
import { openInNewTab } from "../../lib/openInNewTab";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import type { FileRefSession } from "../../lib/fileRef";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { AcpFileRefContext } from "./AcpFileRefContext";
import { ToolCard } from "./ToolCards";
import { makeCompletion, makeError, makeToolCall } from "./__fixtures__/toolCalls";

const session: FileRefSession = { id: "s1", project_path: "/tmp", main_repo_path: null, workspace_repos: [] };
const getFile = vi.mocked(getSessionFile);
const PLACEHOLDER = makeCompletion({ text: "completed" });
const CURRENT_FILE_NOTE = "Current file contents";
const TRUNCATED_NOTE = "Truncated by the server";

beforeEach(() => getFile.mockReset());
afterEach(cleanup);

function readCard(path: string, result: ActivityRow | undefined, extra: Record<string, unknown> = {}) {
  const tool: ToolCall = makeToolCall({ kind: "read", args_preview: JSON.stringify({ file_path: path, ...extra }) });
  const rendered = render(
    <AcpFileRefContext.Provider value={{ fileRefSession: session }}>
      <AgentProfileProvider toolKey={null}>
        <ToolCard tool={tool} result={result} />
      </AgentProfileProvider>
    </AcpFileRefContext.Provider>,
  );
  // Read cards start collapsed; the result mounts, and loads, on expand.
  const toggle = rendered.container.querySelector("button");
  if (toggle) fireEvent.click(toggle);
  return rendered;
}

describe("read card with an empty completion", () => {
  it("shows the file text inline, honoring the offset and limit", async () => {
    getFile.mockResolvedValue({ content: "alpha\nbeta\ngamma\ndelta\n", is_binary: false, truncated: false });
    const { container } = readCard("/tmp/a.txt", PLACEHOLDER, { offset: 2, limit: 2 });
    await waitFor(() => expect(container.textContent).toContain("beta\ngamma"));
    expect(container.textContent).not.toContain("alpha");
    expect(container.textContent).not.toContain("delta");
    expect(getFile).toHaveBeenCalledWith("s1", "/tmp/a.txt");
  });

  it.each<[string, string, { content: string; is_binary: boolean }]>([
    ["text", "/tmp/a.txt", { content: "alpha\n", is_binary: false }],
    ["an image", "/tmp/shot.png", { content: "", is_binary: true }],
    ["another file", "/tmp/doc.pdf", { content: "", is_binary: true }],
  ])("labels %s as the current file, not the recorded Read output", async (_label, path, file) => {
    getFile.mockResolvedValue({ ...file, truncated: false });
    const { container } = readCard(path, PLACEHOLDER);
    await waitFor(() => expect(container.textContent).toContain(CURRENT_FILE_NOTE));
  });

  it("does not label agent-sent output as the current file", () => {
    const { container } = readCard("/tmp/a.txt", makeCompletion({ text: "1\tkept" }));
    expect(container.textContent).not.toContain(CURRENT_FILE_NOTE);
  });

  it.each<[string, Record<string, unknown>, boolean]>([
    ["an unrestricted read", {}, true],
    ["a range crossing the cut", { offset: 2, limit: 5 }, true],
    ["a range inside the returned prefix", { offset: 1, limit: 2 }, false],
  ])("flags a server-truncated result for %s only when lines are missing", async (_label, range, flagged) => {
    getFile.mockResolvedValue({ content: "alpha\nbeta\ngamma\n", is_binary: false, truncated: true });
    const { container } = readCard("/tmp/big.log", PLACEHOLDER, range);
    await waitFor(() => expect(container.textContent).toContain(CURRENT_FILE_NOTE));
    expect(container.textContent?.includes(TRUNCATED_NOTE)).toBe(flagged);
  });

  it("keeps the Markdown fence of a file that is one fenced block", async () => {
    getFile.mockResolvedValue({ content: "```\nfenced\n```\n", is_binary: false, truncated: false });
    const { container } = readCard("/tmp/notes.md", PLACEHOLDER);
    await waitFor(() => expect(container.textContent).toContain("fenced"));
    expect(container.textContent).toContain("```\nfenced\n```");
  });

  it("says so when the range lies beyond a truncated prefix", async () => {
    getFile.mockResolvedValue({ content: "alpha\nbeta\n", is_binary: false, truncated: true });
    const { container } = readCard("/tmp/big.log", PLACEHOLDER, { offset: 900 });
    await waitFor(() => expect(container.textContent).toContain("beyond"));
    expect(container.querySelector("pre")).toBeNull();
  });

  it("shows a blank line inside a truncated prefix instead of calling it unavailable", async () => {
    getFile.mockResolvedValue({ content: "alpha\n\nbeta\n", is_binary: false, truncated: true });
    const { container } = readCard("/tmp/big.log", PLACEHOLDER, { offset: 2, limit: 1 });
    await waitFor(() => expect(container.textContent).toContain(CURRENT_FILE_NOTE));
    expect(container.textContent).not.toContain("beyond");
    expect(container.textContent).not.toContain(TRUNCATED_NOTE);
  });

  it("offers a PDF as a chip even when it holds no NUL byte", async () => {
    getFile.mockResolvedValue({ content: "%PDF-1.4 text only", is_binary: false, truncated: false });
    const { container } = readCard("/tmp/doc.pdf", PLACEHOLDER);
    await waitFor(() => expect(container.querySelector(".lucide-file-down")).not.toBeNull());
    expect(container.textContent).not.toContain("%PDF");
  });

  it("expands an image in the lightbox", async () => {
    getFile.mockResolvedValue({ content: "", is_binary: true, truncated: false });
    const { container, findByTestId } = readCard("/tmp/shot.png", PLACEHOLDER);
    const img = await waitFor(() => {
      const el = container.querySelector("img");
      expect(el).not.toBeNull();
      return el!;
    });
    expect(img.getAttribute("src")).toBe("blob:/api/sessions/s1/file/raw?path=%2Ftmp%2Fshot.png");
    fireEvent.click(container.querySelector('button[title="Expand"]')!);
    expect(await findByTestId("image-lightbox")).not.toBeNull();
  });

  it("offers other binary files as an openable chip", async () => {
    getFile.mockResolvedValue({ content: "", is_binary: true, truncated: false });
    const { container } = readCard("/tmp/doc.pdf", PLACEHOLDER);
    const chip = await waitFor(() => {
      const el = container.querySelector(".lucide-file-down")?.closest("button");
      expect(el).toBeTruthy();
      return el!;
    });
    fireEvent.click(chip);
    expect(openInNewTab).toHaveBeenCalledWith("/api/sessions/s1/file/raw?path=%2Ftmp%2Fdoc.pdf", "doc.pdf");
  });

  it("keeps the plain card when the server refuses the file", async () => {
    getFile.mockResolvedValue(null);
    const { container } = readCard("/tmp/a.txt", PLACEHOLDER);
    await waitFor(() => expect(getFile).toHaveBeenCalled());
    expect(container.querySelector("pre")).toBeNull();
    expect(container.querySelector("img")).toBeNull();
  });

  it.each<[string, ActivityRow | undefined]>([
    ["agent-sent text", makeCompletion({ text: "1\tkept" })],
    ["a failure", makeError()],
    ["a running call", undefined],
    ["tool output blocks", makeCompletion({ text: "completed", output: [{ kind: "text", text: "blocks" }] })],
  ])("does not fetch for %s", async (_label, result) => {
    readCard("/tmp/a.txt", result);
    await Promise.resolve();
    expect(getFile).not.toHaveBeenCalled();
  });
});
