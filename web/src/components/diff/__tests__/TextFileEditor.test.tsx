// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { TextFileEditor } from "../TextFileEditor";
import * as api from "../../../lib/api";
import * as clipboard from "../../../lib/clipboard";
import {
  confirmFileEditNavigation,
  confirmFileEditRemoval,
  fileDraftKey,
  hasUnsavedFileEdits,
  installFileEditGuards,
  isFileEditorOpen,
  preserveLineEndings,
  registerFileEditor,
  setFileDraft,
} from "../../../lib/fileEditGuard";

installFileEditGuards();
afterEach(() => {
  cleanup();
  setFileDraft(fileDraftKey("s1", "notes.txt"), null);
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
  it("keeps one pending save across remounts and delivers success to the current editor", async () => {
    let complete!: (value: { ok: boolean; status: number; error: undefined }) => void;
    const save = vi.spyOn(api, "saveSessionFile").mockImplementation(
      () =>
        new Promise((resolve) => {
          complete = resolve;
        }),
    );
    const first = setup();
    fireEvent.change(first.editor, { target: { value: "pending draft" } });
    fireEvent.keyDown(first.editor, { key: "s", ctrlKey: true });
    cleanup();
    expect(hasUnsavedFileEdits()).toBe(true);
    expect(confirmFileEditNavigation()).toBe(false);
    const current = setup();
    expect((current.editor as HTMLTextAreaElement).value).toBe("pending draft");
    expect((current.editor as HTMLTextAreaElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Saving..." }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.keyDown(current.editor, { key: "s", ctrlKey: true });
    expect(save).toHaveBeenCalledOnce();
    expect(confirmFileEditNavigation()).toBe(false);
    await act(async () => {
      complete({ ok: true, status: 204, error: undefined });
    });
    expect(first.onSaved).not.toHaveBeenCalled();
    expect(current.onSaved).toHaveBeenCalledOnce();
    expect(hasUnsavedFileEdits()).toBe(false);
  });

  it("unblocks navigation and unload when a save completes while no editor is mounted", async () => {
    let complete!: (value: { ok: boolean; status: number; error: undefined }) => void;
    vi.spyOn(api, "saveSessionFile").mockImplementation(
      () =>
        new Promise((resolve) => {
          complete = resolve;
        }),
    );
    const first = setup();
    fireEvent.change(first.editor, { target: { value: "saved offscreen" } });
    fireEvent.keyDown(first.editor, { key: "s", ctrlKey: true });
    cleanup();
    expect(hasUnsavedFileEdits()).toBe(true);
    expect(confirmFileEditNavigation()).toBe(false);
    await act(async () => {
      complete({ ok: true, status: 204, error: undefined });
    });
    const confirm = vi.spyOn(window, "confirm");
    expect(confirmFileEditNavigation()).toBe(true);
    expect(hasUnsavedFileEdits()).toBe(false);
    expect(confirm).not.toHaveBeenCalled();
    const unload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(false);
    const current = setup();
    await waitFor(() => expect(current.onSaved).toHaveBeenCalledOnce());
    expect(first.onSaved).not.toHaveBeenCalled();
  });

  it.each(["conflict", "offline"])("restores a save's %s result after it settles while unmounted", async (failure) => {
    let complete!: (value: { ok: boolean; status: number; error: string }) => void;
    let reject!: (reason: Error) => void;
    const save = vi.spyOn(api, "saveSessionFile").mockImplementation(
      () =>
        new Promise((resolve, fail) => {
          complete = resolve;
          reject = fail;
        }),
    );
    const first = setup();
    fireEvent.change(first.editor, { target: { value: "keep pending draft" } });
    fireEvent.keyDown(first.editor, { key: "s", ctrlKey: true });
    cleanup();
    await act(async () => {
      if (failure === "conflict") complete({ ok: false, status: 409, error: "File changed" });
      else reject(new Error("offline"));
    });
    const current = setup();
    await screen.findByText(failure === "conflict" ? "File changed" : /Save could not be confirmed/);
    expect((current.editor as HTMLTextAreaElement).value).toBe("keep pending draft");
    expect((current.editor as HTMLTextAreaElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Save", exact: true }) as HTMLButtonElement).disabled).toBe(
      failure === "conflict",
    );
    expect(save).toHaveBeenCalledOnce();
    expect(first.onSaved).not.toHaveBeenCalled();
    expect(current.onSaved).not.toHaveBeenCalled();
    vi.spyOn(window, "confirm").mockReturnValue(false);
    expect(confirmFileEditNavigation()).toBe(false);
  });

  it("transfers draft ownership between panes without unregistering the new owner", () => {
    const key = fileDraftKey("s1", "notes.txt");
    const first = vi.fn();
    const current = vi.fn();
    const releaseFirst = registerFileEditor(key, first);
    const releaseCurrent = registerFileEditor(key, current);
    try {
      expect(first).toHaveBeenCalledOnce();
      releaseFirst();
      setFileDraft(key, { original, text: "shared draft", pendingSave: null });
      vi.spyOn(window, "confirm").mockReturnValue(true);
      expect(confirmFileEditNavigation()).toBe(true);
      expect(current).toHaveBeenCalledOnce();
      expect(first).toHaveBeenCalledOnce();
    } finally {
      releaseFirst();
      releaseCurrent();
    }
  });

  it("owns Escape only while mounted, so the app shortcut does not also close the file", () => {
    setup();
    expect(isFileEditorOpen()).toBe(true);
    cleanup();
    expect(isFileEditorOpen()).toBe(false);
  });
  it("keeps an unsaved draft through a remount, as a layout switch does", () => {
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "survives" } });
    cleanup();
    expect(isFileEditorOpen()).toBe(false);
    // Switching views parks the draft without asking; only an unload would lose it.
    expect(hasUnsavedFileEdits()).toBe(false);
    const unload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(true);
    expect((setup().editor as HTMLTextAreaElement).value).toBe("survives");
    expect(hasUnsavedFileEdits()).toBe(true);
  });
  it("confirms removal of a Session with a parked draft without blocking removal of another Session", () => {
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "parked draft" } });
    cleanup();
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    expect(confirmFileEditRemoval(["s2"])).toBe(true);
    expect(confirm).not.toHaveBeenCalled();
    expect(confirmFileEditRemoval(["s1"])).toBe(false);
    expect(confirm).toHaveBeenCalledOnce();
    confirm.mockReturnValue(true);
    expect(confirmFileEditRemoval(["s1"])).toBe(true);
    const unload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(false);
  });

  it("leaves no draft behind once the edit is clean again", () => {
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "changed" } });
    fireEvent.change(editor, { target: { value: "one\ntwo\n" } });
    cleanup();
    const unload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(false);
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
  it.each(["Cannot verify this Sandbox's shared files", "This filesystem does not support atomic file replacement"])(
    "keeps the draft and disables saving until edit eligibility recovers: %s",
    async (reason) => {
      vi.useFakeTimers();
      vi.spyOn(api, "getSessionFile")
        .mockResolvedValueOnce({ ...original, edit_disabled_reason: reason })
        .mockResolvedValue(original);
      const save = vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: true, status: 204, error: undefined });
      const { editor, onCancel, onSaved } = setup();
      fireEvent.change(editor, { target: { value: "keep draft" } });
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      expect(screen.getByText(reason)).toBeTruthy();
      expect(screen.queryByText(/File changed on disk/)).toBeNull();
      expect((screen.getByRole("button", { name: "Save", exact: true }) as HTMLButtonElement).disabled).toBe(true);
      fireEvent.keyDown(editor, { key: "s", ctrlKey: true });
      expect(save).not.toHaveBeenCalled();
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      expect(screen.queryByText(reason)).toBeNull();
      expect((editor as HTMLTextAreaElement).value).toBe("keep draft");
      expect((screen.getByRole("button", { name: "Save", exact: true }) as HTMLButtonElement).disabled).toBe(false);
      expect(onCancel).not.toHaveBeenCalled();
      await act(async () => {
        fireEvent.keyDown(editor, { key: "s", metaKey: true });
      });
      expect(onSaved).toHaveBeenCalledOnce();
      expect(save).toHaveBeenCalledWith("s1", "notes.txt", "v1", "keep draft");
    },
  );

  it("keeps real version conflicts latched after edit eligibility recovers", async () => {
    vi.useFakeTimers();
    vi.spyOn(api, "getSessionFile")
      .mockResolvedValueOnce({ ...original, version: "v2", edit_disabled_reason: "Sandbox restarting" })
      .mockResolvedValue(original);
    const save = vi.spyOn(api, "saveSessionFile");
    const { editor } = setup();
    fireEvent.change(editor, { target: { value: "keep draft" } });
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByText(/File changed on disk/)).toBeTruthy();
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.queryByText("Sandbox restarting")).toBeNull();
    expect(screen.getByText(/File changed on disk/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Save", exact: true }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.keyDown(editor, { key: "s", ctrlKey: true });
    expect(save).not.toHaveBeenCalled();
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
