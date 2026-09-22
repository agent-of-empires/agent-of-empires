// @vitest-environment jsdom
// Hardware keyboard chords, key sequences, and paste in the live terminal.

import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, waitFor } from "@testing-library/react";
import {
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  forwardTerminalBeforeInput,
} from "../../lib/mobileKeyboardProxy";
import { installResizeObserver, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
const writeClipboard = vi.fn();
vi.mock("../../lib/clipboard", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/clipboard")>()),
  writeClipboard: (text: string) => writeClipboard(text),
}));
installResizeObserver();

function renderTerm(uploadPastedImage: (f: File) => Promise<string | null> = vi.fn(async () => null), ctrl = false) {
  const ctrlActiveRef = { current: ctrl };
  const sendData = vi.fn<(data: string) => boolean>(() => true);
  const view = renderLiveTerminal({
    sendData,
    uploadPastedImage,
    ctrlActiveRef,
    clearCtrl: () => {
      ctrlActiveRef.current = false;
    },
  });
  return { input: view.input(), sendData, unmount: view.unmount };
}

const imageItem = (file: File) =>
  ({ kind: "file", type: file.type, getAsFile: () => file }) as unknown as DataTransferItem;
const png = (name = "s.png") => new File([new Uint8Array([1])], name, { type: "image/png" });
const clipboard = (text: string, items: DataTransferItem[] = []) => ({
  clipboardData: { getData: (t: string) => (t === "text/plain" ? text : ""), items },
});
const withProxy = (value: string, run: (proxy: HTMLTextAreaElement) => void) => {
  const proxy = document.createElement("textarea");
  proxy.dataset.keyboardProxy = "";
  proxy.value = value;
  document.body.append(proxy);
  try {
    run(proxy);
  } finally {
    proxy.remove();
  }
};

describe("MobileLiveTerminal paste", () => {
  it("lets Ctrl+V reach the native paste event, which sends a bracketed paste (#2384)", () => {
    const { input, sendData } = renderTerm();
    expect(fireEvent.keyDown(input, { key: "v", ctrlKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalledWith("\x16");
    fireEvent.paste(input, clipboard("hello world"));
    expect(sendData).toHaveBeenCalledWith("\x1b[200~hello world\x1b[201~");
  });

  it.each([
    [
      "the host path of an uploaded image (#2678)",
      "",
      "/repo/.aoe-pasted-images/x.png",
      " /repo/.aoe-pasted-images/x.png ",
    ],
    ["an escaped path with spaces", "", "/Users/me/Agent of Empires/x.png", " /Users/me/Agent\\ of\\ Empires/x.png "],
    ["clipboard text beside the path", "look at", "/repo/x.png", " look at /repo/x.png "],
  ])("pastes %s", async (_n, text, path, pasted) => {
    const upload = vi.fn(async () => path);
    const { input, sendData } = renderTerm(upload);
    const file = png();
    fireEvent.paste(input, clipboard(text, [imageItem(file)]));
    expect(upload).toHaveBeenCalledWith(file);
    await vi.waitFor(() => expect(sendData).toHaveBeenCalledWith(`\x1b[200~${pasted}\x1b[201~`));
  });

  it("sends nothing when the image upload fails", async () => {
    const upload = vi.fn(async () => null);
    const { input, sendData } = renderTerm(upload);
    fireEvent.paste(input, clipboard("", [imageItem(png())]));
    await vi.waitFor(() => expect(upload).toHaveBeenCalled());
    expect(sendData).not.toHaveBeenCalled();
  });

  it("drops a retained syllable from both shadows when a paste bypasses the textarea", () => {
    withProxy("한", (proxy) => {
      const { input, sendData } = renderTerm();
      input.value = "한";
      fireEvent(
        input,
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertFromPaste",
          data: "ls -al",
        }),
      );
      expect(sendData).toHaveBeenCalledWith(expect.stringContaining("ls -al"));
      expect(input.value).toBe("");
      expect(proxy.value).toBe("");
    });
  });

  it("does not touch another session's proxy after the uploading terminal unmounts", async () => {
    let finish!: (path: string) => void;
    const pending = new Promise<string>((resolve) => (finish = resolve));
    const { input, sendData, unmount } = renderTerm(() => pending);
    fireEvent.paste(input, clipboard("", [imageItem(png("shot.png"))]));
    unmount();
    const proxy = document.createElement("textarea");
    proxy.dataset.keyboardProxy = "";
    proxy.value = "ㅎ";
    document.body.append(proxy);
    try {
      await act(async () => {
        finish("/tmp/paste.png");
        await pending;
      });
      expect(proxy.value).toBe("ㅎ");
      expect(sendData).not.toHaveBeenCalled();
    } finally {
      proxy.remove();
    }
  });

  it("retains accepted replay after a Ctrl chord for the next Korean rewrite", () => {
    clearMobileKeyboardProxyInput();
    withProxy("cㅎ", (proxy) => {
      deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "c", isComposing: false });
      deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "ㅎ", isComposing: false });
      const { sendData } = renderTerm(undefined, true);
      const sent = () => sendData.mock.calls.map(([data]) => data).join("");
      expect(sent()).toBe("\x03ㅎ");
      expect(proxy.value).toBe("ㅎ");
      proxy.addEventListener("beforeinput", (event) =>
        forwardTerminalBeforeInput(event as InputEvent, deliverMobileKeyboardProxyInput),
      );
      const input = (init: InputEventInit) =>
        proxy.dispatchEvent(new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init }));
      expect(input({ inputType: "deleteContentBackward" })).toBe(true);
      proxy.value = "";
      expect(input({ inputType: "insertText", data: "하" })).toBe(true);
      expect(sent()).toBe("\x03ㅎ\x7f하");
    });
    clearMobileKeyboardProxyInput();
  });
});

describe("MobileLiveTerminal key sequences", () => {
  it.each([
    ["Enter", {}, "\r"],
    ["Enter", { altKey: true }, "\r"],
    // Shift and Ctrl Enter insert a soft newline for agents.
    ["Enter", { ctrlKey: true }, "\x1b\r"],
    ["Enter", { shiftKey: true }, "\x1b\r"],
    ["Enter", { ctrlKey: true, shiftKey: true }, "\x1b\r"],
    ["Backspace", { altKey: true }, "\x1b\x7f"],
    ["Backspace", { ctrlKey: true }, "\x7f"],
    ["Tab", {}, "\t"],
    ["Tab", { shiftKey: true }, "\x1b[Z"],
    ["Escape", {}, "\x1b"],
    ["ArrowUp", {}, "\x1b[A"],
    ["ArrowDown", {}, "\x1b[B"],
    ["ArrowRight", {}, "\x1b[C"],
    ["ArrowLeft", {}, "\x1b[D"],
    ["Insert", {}, "\x1b[2~"],
    ["Delete", {}, "\x1b[3~"],
    ["Home", {}, "\x1b[H"],
    ["End", {}, "\x1b[F"],
    ["PageUp", {}, "\x1b[5~"],
    ["PageDown", {}, "\x1b[6~"],
    ["ArrowUp", { shiftKey: true }, "\x1b[1;2A"],
    ["ArrowDown", { altKey: true }, "\x1b[1;3B"],
    ["ArrowRight", { altKey: true }, "\x1b[1;3C"],
    ["ArrowLeft", { ctrlKey: true }, "\x1b[1;5D"],
    ["Home", { ctrlKey: true }, "\x1b[1;5H"],
    ["End", { ctrlKey: true, shiftKey: true }, "\x1b[1;6F"],
    ["Insert", { shiftKey: true }, "\x1b[2;2~"],
    ["PageUp", { altKey: true }, "\x1b[5;3~"],
    ["PageDown", { altKey: true }, "\x1b[6;3~"],
    ["Delete", { ctrlKey: true }, "\x1b[3;5~"],
    ["c", { ctrlKey: true }, "\x03"],
    ["v", { code: "KeyV", altKey: true }, "\x1bv"],
    ["V", { code: "KeyV", altKey: true, shiftKey: true }, "\x1bV"],
    // Option+V composes a symbol; the physical code recovers the letter.
    ["√", { code: "KeyV", altKey: true }, "\x1bv"],
  ])("%s %o sends %j", (key, init, expected) => {
    const { input, sendData } = renderTerm();
    expect(fireEvent.keyDown(input, { key, ...init })).toBe(false);
    expect(sendData).toHaveBeenCalledWith(expected);
  });

  it.each([
    ["Meta navigation", { key: "ArrowLeft", metaKey: true }],
    ["macOS dead keys", { key: "Dead", code: "KeyE", altKey: true }],
    ["Ctrl+Alt printable chords (AltGr)", { key: "v", ctrlKey: true, altKey: true }],
  ])("leaves %s to the browser", (_n, init) => {
    const { input, sendData } = renderTerm();
    expect(fireEvent.keyDown(input, init)).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("sends no Alt chord during an IME composition", () => {
    const { input, sendData } = renderTerm();
    fireEvent.compositionStart(input);
    expect(fireEvent.keyDown(input, { key: "v", code: "KeyV", altKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("prefers the Keyboard Layout Map over the physical code", async () => {
    const original = Object.getOwnPropertyDescriptor(navigator, "keyboard");
    const getLayoutMap = vi.fn().mockResolvedValue(new Map([["KeyQ", "a"]]));
    Object.defineProperty(navigator, "keyboard", { configurable: true, value: { getLayoutMap } });
    try {
      const { input, sendData } = renderTerm();
      await waitFor(() => expect(getLayoutMap).toHaveBeenCalled());
      expect(fireEvent.keyDown(input, { key: "æ", code: "KeyQ", altKey: true })).toBe(false);
      expect(sendData).toHaveBeenCalledWith("\x1ba");
    } finally {
      if (original) Object.defineProperty(navigator, "keyboard", original);
      else delete (navigator as Navigator & { keyboard?: unknown }).keyboard;
    }
  });

  it.each([
    ["copies the selection", "selected output", ["selected output"]],
    ["is a no-op without a selection", "", []],
  ])("Ctrl+Shift+C %s and never sends ^C", (_n, selected, copied) => {
    writeClipboard.mockClear();
    const spy = vi.spyOn(window, "getSelection").mockReturnValue({ toString: () => selected } as unknown as Selection);
    try {
      const { input, sendData } = renderTerm();
      fireEvent.keyDown(input, { key: "C", ctrlKey: true, shiftKey: true });
      expect(writeClipboard.mock.calls.map(([t]) => t)).toEqual(copied);
      expect(sendData).not.toHaveBeenCalledWith("\x03");
    } finally {
      spy.mockRestore();
    }
  });
});
