// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { TextFileEditor } from "../TextFileEditor";
import * as api from "../../../lib/api";
import * as clipboard from "../../../lib/clipboard";
import { confirmFileEditNavigation, isFileEditorOpen, preserveLineEndings } from "../../../lib/fileEditGuard";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});
const original = { content: "one\r\ntwo\r\n", version: "v1", is_binary: false, truncated: false };
function setup() {
  const onSaved = vi.fn();
  const onCancel = vi.fn();
  render(
    <TextFileEditor sessionId="s1" filePath="notes.txt" original={original} onSaved={onSaved} onCancel={onCancel} />,
  );
  return { onSaved, onCancel, editor: screen.getByRole("textbox", { name: "Edit notes.txt" }) };
}

describe("TextFileEditor", () => {
  it("allows only one disk check in flight across repeated focus events", async () => {
    vi.useFakeTimers();
    let resolve!: (value: typeof original) => void;
    const check = vi.spyOn(api, "getSessionFile").mockImplementation(
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    setup();
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(check).toHaveBeenCalledTimes(1);
    fireEvent.focus(window);
    fireEvent.focus(window);
    expect(check).toHaveBeenCalledTimes(1);
    await act(async () => {
      resolve({ ...original, version: "v2" });
    });
    expect(screen.getByText("File changed on disk. Copy your edits or reload before saving.")).toBeTruthy();
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(check).toHaveBeenCalledTimes(2);
  });

  it("sends the opened version and preserves CRLF and the final newline", async () => {
    const save = vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: true, status: 204, error: undefined });
    const { editor, onSaved } = setup();
    fireEvent.change(editor, { target: { value: "changed\ntwo\n" } });
    fireEvent.keyDown(editor, { key: "s", ctrlKey: true });
    await waitFor(() => expect(onSaved).toHaveBeenCalledOnce());
    expect(save).toHaveBeenCalledWith("s1", "notes.txt", "v1", "changed\r\ntwo\r\n");
  });
  it("keeps a conflicting draft and offers a confirmed reload without reporting a save", async () => {
    vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: false, status: 409, error: "File changed" });
    const { editor, onCancel, onSaved } = setup();
    fireEvent.change(editor, { target: { value: "my edit" } });
    fireEvent.click(screen.getByRole("button", { name: "Save", exact: true }));
    await screen.findByText("File changed on disk. Copy your edits or reload before saving.");
    expect((editor as HTMLTextAreaElement).value).toBe("my edit");
    expect((screen.getByRole("button", { name: "Save", exact: true }) as HTMLButtonElement).disabled).toBe(true);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    fireEvent.click(screen.getByRole("button", { name: "Reload from disk" }));
    expect(onCancel).not.toHaveBeenCalled();
    confirm.mockReturnValue(true);
    fireEvent.click(screen.getByRole("button", { name: "Reload from disk" }));
    expect(onCancel).toHaveBeenCalledOnce();
    expect(onSaved).not.toHaveBeenCalled();
  });
  it("retains a draft after network failure and refuses navigation until confirmed", async () => {
    vi.spyOn(api, "saveSessionFile").mockRejectedValue(new Error("offline"));
    const { editor, onCancel } = setup();
    fireEvent.change(editor, { target: { value: "keep me" } });
    fireEvent.click(screen.getByRole("button", { name: "Save", exact: true }));
    await screen.findByText(/Save could not be confirmed/);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    expect(confirmFileEditNavigation()).toBe(false);
    expect(onCancel).not.toHaveBeenCalled();
    confirm.mockReturnValue(true);
    expect(confirmFileEditNavigation()).toBe(true);
    expect(onCancel).toHaveBeenCalledOnce();
    expect(confirmFileEditNavigation()).toBe(true);
    expect(confirm).toHaveBeenCalledTimes(2);
  });
  it("blocks duplicate saves and leaving until the pending write completes", async () => {
    let complete!: (value: { ok: boolean; status: number; error: undefined }) => void;
    const save = vi.spyOn(api, "saveSessionFile").mockImplementation(
      () =>
        new Promise((resolve) => {
          complete = resolve;
        }),
    );
    const confirm = vi.spyOn(window, "confirm");
    const { editor, onSaved, onCancel } = setup();
    fireEvent.change(editor, { target: { value: "pending" } });
    fireEvent.keyDown(editor, { key: "s", metaKey: true });
    fireEvent.keyDown(editor, { key: "s", ctrlKey: true });
    expect(save).toHaveBeenCalledOnce();
    expect(confirmFileEditNavigation()).toBe(false);
    expect(confirm).not.toHaveBeenCalled();
    fireEvent.keyDown(editor, { key: "Escape" });
    expect(onCancel).not.toHaveBeenCalled();
    await act(async () => complete({ ok: true, status: 204, error: undefined }));
    expect(onSaved).toHaveBeenCalledOnce();
  });
  it("owns Escape only while mounted, so the app shortcut does not also close the file", () => {
    setup();
    expect(isFileEditorOpen()).toBe(true);
    cleanup();
    expect(isFileEditorOpen()).toBe(false);
  });
  it("warns on tab unload only while edits are dirty", () => {
    const { editor } = setup();
    const clean = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(clean);
    expect(clean.defaultPrevented).toBe(false);
    fireEvent.change(editor, { target: { value: "draft" } });
    const dirty = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(dirty);
    expect(dirty.defaultPrevented).toBe(true);
  });
  it("copies conflicting edits and reports clipboard failure without losing text", async () => {
    vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: false, status: 409, error: "Conflict" });
    const copy = vi.spyOn(clipboard, "writeClipboard").mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "my text" } });
    fireEvent.click(screen.getByRole("button", { name: "Save", exact: true }));
    fireEvent.click(await screen.findByRole("button", { name: "Copy my edits" }));
    await screen.findByText("Could not copy. Select your text and copy it manually.");
    expect((editor as HTMLTextAreaElement).value).toBe("my text");
    fireEvent.click(screen.getByRole("button", { name: "Copy my edits" }));
    await screen.findByRole("button", { name: "Copied" });
    expect(copy).toHaveBeenLastCalledWith("my text");
  });
  it("clears a failed disk-check notice after recovery while keeping the draft", async () => {
    vi.useFakeTimers();
    vi.spyOn(api, "getSessionFile").mockResolvedValueOnce(null).mockResolvedValue(original);
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "keep draft" } });
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByText("Cannot check disk changes. Your edits are kept.")).toBeTruthy();
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.queryByText("Cannot check disk changes. Your edits are kept.")).toBeNull();
    expect((editor as HTMLTextAreaElement).value).toBe("keep draft");
  });
  it.each([
    ["a\nb", "a\r\nb", "a\r\nb"],
    ["a\nb\n", "a\nb\n", "a\nb\n"],
    ["a\nb\nc\n", "a\r\nb\nc\r", "a\r\nb\nc\r"],
    ["a\nb\nc\n", "a\r\n", "a\r\nb\r\nc\r\n"],
  ])("preserves original endings and newline intent", (edited, before, expected) => {
    expect(preserveLineEndings(edited, before)).toBe(expected);
  });
});
