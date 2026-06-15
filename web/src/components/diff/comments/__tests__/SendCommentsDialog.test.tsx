// @vitest-environment jsdom
//
// Tests for SendCommentsDialog: the three-piece compose dialog that
// forwards diff review comments to the ACP worker. Cover compose/submit
// (asserting the POST payload), the empty/disabled state, cancel, the
// Cmd+Enter / Escape hotkeys, and the failure path.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SendCommentsDialog } from "../SendCommentsDialog";
import type { DiffComment } from "../types";

const reportTelemetrySeen = vi.fn();
vi.mock("../../../../lib/api", () => ({
  reportTelemetrySeen: (...args: unknown[]) => reportTelemetrySeen(...args),
}));

const fetchMock = vi.fn();

function comment(overrides?: Partial<DiffComment>): DiffComment {
  return {
    id: "c1",
    filePath: "src/foo.ts",
    side: "new",
    startLine: 10,
    endLine: 10,
    body: "Rename this",
    capturedSnippet: "const x = 1;",
    createdAt: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function setup(overrides?: {
  comments?: DiffComment[];
  isMultiRepo?: boolean;
  sendEnabled?: boolean;
  sendDisabledReason?: string;
  introDraft?: string;
  outroDraft?: string;
  clearAfterSend?: boolean;
}) {
  const onChangeIntro = vi.fn();
  const onChangeOutro = vi.fn();
  const onChangeClearAfterSend = vi.fn();
  const onClose = vi.fn();
  const onSent = vi.fn();
  const utils = render(
    <SendCommentsDialog
      sessionId="sess 1"
      comments={overrides?.comments ?? [comment()]}
      isMultiRepo={overrides?.isMultiRepo ?? false}
      sendEnabled={overrides?.sendEnabled ?? true}
      sendDisabledReason={overrides?.sendDisabledReason}
      introDraft={overrides?.introDraft ?? ""}
      outroDraft={overrides?.outroDraft ?? ""}
      clearAfterSend={overrides?.clearAfterSend ?? false}
      onChangeIntro={onChangeIntro}
      onChangeOutro={onChangeOutro}
      onChangeClearAfterSend={onChangeClearAfterSend}
      onClose={onClose}
      onSent={onSent}
    />,
  );
  return { ...utils, onChangeIntro, onChangeOutro, onChangeClearAfterSend, onClose, onSent };
}

function sendButton(container: HTMLElement): HTMLButtonElement {
  return Array.from(container.querySelectorAll("button")).find((b) =>
    /^(Send|Sending)/.test(b.textContent?.trim() ?? ""),
  ) as HTMLButtonElement;
}

beforeEach(() => {
  reportTelemetrySeen.mockClear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SendCommentsDialog", () => {
  it("renders the comment count and a preview of the assembled markdown", () => {
    const { container } = setup({ comments: [comment({ body: "Rename this" })] });
    expect(container.textContent).toContain("Send diff comments");
    expect(container.textContent).toContain("1 comment");
    expect(container.textContent).toContain("Rename this");
  });

  it("pluralizes the comment count", () => {
    const { container } = setup({
      comments: [comment({ id: "a" }), comment({ id: "b", startLine: 20, endLine: 20 })],
    });
    expect(container.textContent).toContain("2 comments");
  });

  it("shows the empty-state preview and disables Send when there are no comments", () => {
    const { container } = setup({ comments: [] });
    expect(container.textContent).toContain("No comments.");
    expect(sendButton(container).disabled).toBe(true);
  });

  it("disables Send and exposes the reason when sendEnabled is false", () => {
    const { container } = setup({ sendEnabled: false, sendDisabledReason: "worker not running" });
    const btn = sendButton(container);
    expect(btn.disabled).toBe(true);
    expect(btn.getAttribute("title")).toBe("worker not running");
  });

  it("submits the assembled prompt payload to the diff-comments endpoint and fires onSent", async () => {
    fetchMock.mockResolvedValue({ ok: true });
    const { container, onSent } = setup({
      comments: [comment({ body: "fix me" })],
      introDraft: "  hello  ",
      outroDraft: "",
    });

    fireEvent.click(sendButton(container));

    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    // sessionId is URL-encoded.
    expect(url).toBe("/api/sessions/sess%201/acp/prompt/diff-comments");
    expect(init.method).toBe("POST");
    expect(init.headers["Content-Type"]).toBe("application/json");
    const body = JSON.parse(init.body);
    // Intro is trimmed; blank outro falls back to the default.
    expect(body.intro).toBe("hello");
    expect(body.outro).toBe("Please address these comments.");
    expect(body.isMultiRepo).toBe(false);
    expect(body.comments).toHaveLength(1);
    expect(body.assembledMarkdown).toContain("hello");
    expect(body.assembledMarkdown).toContain("fix me");
    expect(reportTelemetrySeen).toHaveBeenCalledWith("diff_comments");
  });

  it("does not fetch when Send is clicked with no comments", () => {
    const { container } = setup({ comments: [] });
    fireEvent.click(sendButton(container));
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("Cmd+Enter triggers a send", async () => {
    fetchMock.mockResolvedValue({ ok: true });
    const { onSent } = setup();
    fireEvent.keyDown(document, { key: "Enter", metaKey: true });
    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("Escape closes the dialog", () => {
    const { onClose } = setup();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("Cancel button closes the dialog", () => {
    const { container, onClose } = setup();
    const cancelBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Cancel")!;
    fireEvent.click(cancelBtn);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("close (×) button closes the dialog", () => {
    const { container, onClose } = setup();
    const closeBtn = container.querySelector<HTMLButtonElement>('button[aria-label="Close"]')!;
    fireEvent.click(closeBtn);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("clicking the backdrop closes the dialog", () => {
    const { container, onClose } = setup();
    const backdrop = container.querySelector(".fixed.inset-0") as HTMLElement;
    fireEvent.mouseDown(backdrop);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("forwards intro/outro/clearAfterSend edits to their callbacks", () => {
    const { container, onChangeIntro, onChangeOutro, onChangeClearAfterSend } = setup();
    const textareas = container.querySelectorAll("textarea");
    fireEvent.change(textareas[0], { target: { value: "new intro" } });
    expect(onChangeIntro).toHaveBeenCalledWith("new intro");
    fireEvent.change(textareas[1], { target: { value: "new outro" } });
    expect(onChangeOutro).toHaveBeenCalledWith("new outro");
    const checkbox = container.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
    fireEvent.click(checkbox);
    expect(onChangeClearAfterSend).toHaveBeenCalledWith(true);
  });

  it("reflects controlled draft values on the textareas", () => {
    const { container } = setup({ introDraft: "intro text", outroDraft: "outro text" });
    const textareas = container.querySelectorAll("textarea");
    expect((textareas[0] as HTMLTextAreaElement).value).toBe("intro text");
    expect((textareas[1] as HTMLTextAreaElement).value).toBe("outro text");
  });

  it("reflects the controlled clearAfterSend checkbox state", () => {
    const { container } = setup({ clearAfterSend: true });
    const checkbox = container.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
    expect(checkbox.checked).toBe(true);
  });

  it("shows an error and does not fire onSent when the server returns non-ok", async () => {
    fetchMock.mockResolvedValue({
      ok: false,
      status: 500,
      text: () => Promise.resolve("boom"),
    });
    const { container, onSent } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(container.textContent).toContain("Failed to send (500)"));
    expect(container.textContent).toContain("boom");
    expect(onSent).not.toHaveBeenCalled();
    expect(reportTelemetrySeen).not.toHaveBeenCalled();
  });

  it("shows an error on a network rejection", async () => {
    fetchMock.mockRejectedValue(new Error("offline"));
    const { container, onSent } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(container.textContent).toContain("Failed to send: offline"));
    expect(onSent).not.toHaveBeenCalled();
  });

  it("shows the Sending... label and ignores a second click while in flight", async () => {
    let resolveFetch: ((v: { ok: boolean }) => void) | null = null;
    fetchMock.mockReturnValue(
      new Promise((resolve) => {
        resolveFetch = resolve;
      }),
    );
    const { container, onSent } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(sendButton(container).textContent?.trim()).toBe("Sending..."));
    // A second click while busy must not start another request.
    fireEvent.click(sendButton(container));
    expect(fetchMock).toHaveBeenCalledTimes(1);

    resolveFetch!({ ok: true });
    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));
  });

  it("Escape is blocked while a send is in flight", async () => {
    let resolveFetch: ((v: { ok: boolean }) => void) | null = null;
    fetchMock.mockReturnValue(
      new Promise((resolve) => {
        resolveFetch = resolve;
      }),
    );
    const { container, onClose } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(sendButton(container).textContent?.trim()).toBe("Sending..."));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
    resolveFetch!({ ok: true });
  });
});
