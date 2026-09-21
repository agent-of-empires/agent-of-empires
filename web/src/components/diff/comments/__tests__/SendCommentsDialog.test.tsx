// @vitest-environment jsdom

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
      sendDisabledReason={overrides?.sendDisabledReason ?? "session is trashed"}
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
  it.each([
    [[comment({ body: "Rename this" })], ["1 comment", "Rename this", "Send diff comments"]],
    [[comment({ id: "a" }), comment({ id: "b", startLine: 20, endLine: 20 })], ["2 comments"]],
  ])("renders the count and preview", (comments, texts) => {
    const { container } = setup({ comments });
    for (const t of texts) expect(container.textContent).toContain(t);
  });

  it("shows the empty-state preview and disables Send when there are no comments", () => {
    const { container } = setup({ comments: [] });
    expect(container.textContent).toContain("No comments.");
    expect(sendButton(container).getAttribute("aria-disabled")).toBe("true");
  });

  it("disables Send and exposes the reason to pointer and keyboard when sendEnabled is false", async () => {
    const { container } = setup({ sendEnabled: false, sendDisabledReason: "session is trashed" });
    const btn = sendButton(container);
    // aria-disabled keeps the reason reachable by hover and keyboard.
    expect(btn.getAttribute("aria-disabled")).toBe("true");
    expect(btn.disabled).toBe(false);

    const wrapper = btn.parentElement as HTMLElement;
    fireEvent.mouseEnter(wrapper);
    await waitFor(() => expect(document.body.textContent).toContain("session is trashed"));
    fireEvent.mouseLeave(wrapper);
    await waitFor(() => expect(document.body.textContent).not.toContain("session is trashed"));

    btn.focus();
    fireEvent.focus(btn);
    await waitFor(() => expect(document.body.textContent).toContain("session is trashed"));
  });

  it("does not send when the button is aria-disabled", () => {
    const { container } = setup({ sendEnabled: false });
    fireEvent.click(sendButton(container));
    expect(fetchMock).not.toHaveBeenCalled();
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
    expect(url).toBe("/api/sessions/sess%201/acp/prompt/diff-comments");
    expect(init.method).toBe("POST");
    expect(init.headers["Content-Type"]).toBe("application/json");
    const body = JSON.parse(init.body);
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

  it.each<[string, (c: HTMLElement) => void]>([
    ["Escape", () => fireEvent.keyDown(document, { key: "Escape" })],
    [
      "Cancel",
      (c) => fireEvent.click(Array.from(c.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Cancel")!),
    ],
    ["the close button", (c) => fireEvent.click(c.querySelector('button[aria-label="Close"]')!)],
    ["the backdrop", (c) => fireEvent.mouseDown(c.querySelector(".fixed.inset-0")!)],
  ])("closes via %s", (_, act) => {
    const { container, onClose } = setup();
    act(container);
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

  it("reflects controlled drafts and the clearAfterSend checkbox", () => {
    const { container } = setup({ introDraft: "intro text", outroDraft: "outro text", clearAfterSend: true });
    const textareas = container.querySelectorAll("textarea");
    expect([textareas[0]!.value, textareas[1]!.value]).toEqual(["intro text", "outro text"]);
    expect(container.querySelector<HTMLInputElement>('input[type="checkbox"]')!.checked).toBe(true);
  });

  it.each<[string, () => void, string[]]>([
    [
      "a non-ok response",
      () => fetchMock.mockResolvedValue({ ok: false, status: 500, text: () => Promise.resolve("boom") }),
      ["Failed to send (500)", "boom"],
    ],
    ["a network rejection", () => fetchMock.mockRejectedValue(new Error("offline")), ["Failed to send: offline"]],
  ])("shows an error for %s without onSent or telemetry", async (_, seed, texts) => {
    seed();
    const { container, onSent } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(container.textContent).toContain(texts[0]));
    for (const t of texts) expect(container.textContent).toContain(t);
    expect(onSent).not.toHaveBeenCalled();
    expect(reportTelemetrySeen).not.toHaveBeenCalled();
  });

  it("while sending, shows Sending..., ignores a second click and Escape", async () => {
    let resolveFetch: (v: { ok: boolean }) => void = () => {};
    fetchMock.mockReturnValue(new Promise((resolve) => (resolveFetch = resolve)));
    const { container, onSent, onClose } = setup();
    fireEvent.click(sendButton(container));
    await waitFor(() => expect(sendButton(container).textContent?.trim()).toBe("Sending..."));
    fireEvent.click(sendButton(container));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
    resolveFetch({ ok: true });
    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));
  });
});
