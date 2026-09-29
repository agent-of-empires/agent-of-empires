// @vitest-environment jsdom

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MobileTerminalToolbar } from "../MobileTerminalToolbar";
import { HOLD_REPEAT_DELAY_MS, HOLD_REPEAT_INTERVAL_MS } from "../../hooks/useHoldRepeat";
import { DEFAULT_TOOLBAR_KEYS, TOOLBAR_KEY_CATALOG, type ToolbarKeyId } from "../../lib/terminalToolbarKeys";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  delete (navigator as { clipboard?: unknown }).clipboard;
});

function secureContext(value: boolean) {
  Object.defineProperty(window, "isSecureContext", { value, configurable: true });
}

function renderToolbar(
  opts: { keys?: readonly ToolbarKeyId[]; inputEl?: HTMLTextAreaElement | null; keyboardOpen?: boolean } = {},
) {
  const sendData = vi.fn<(data: string) => boolean>(() => true);
  const sendPaste = vi.fn<(text: string, submit: boolean) => boolean>(() => true);
  const onCompose = vi.fn();
  const result = render(
    <MobileTerminalToolbar
      keys={opts.keys ?? TOOLBAR_KEY_CATALOG.map((k) => k.id)}
      sendData={sendData}
      sendPaste={sendPaste}
      onCompose={onCompose}
      inputElRef={{ current: opts.inputEl ?? null }}
      keyboardOpen={opts.keyboardOpen ?? false}
      ctrlActive={false}
      onCtrlToggle={vi.fn()}
    />,
  );
  return { ...result, sendData, sendPaste, onCompose };
}

describe("MobileTerminalToolbar keys", () => {
  it("renders the configured keys in order, and nothing for an empty row", () => {
    renderToolbar({ keys: DEFAULT_TOOLBAR_KEYS });
    expect(screen.getAllByRole("button").map((b) => b.getAttribute("aria-label"))).toEqual([
      "Escape",
      "Tab",
      "Ctrl",
      "Paste from clipboard",
      "Compose",
    ]);
    cleanup();
    renderToolbar({ keys: ["enter", "esc"] });
    expect(screen.getAllByRole("button").map((b) => b.getAttribute("aria-label"))).toEqual(["Enter", "Escape"]);
    cleanup();
    expect(renderToolbar({ keys: [] }).container.firstChild).toBeNull();
  });

  it.each(TOOLBAR_KEY_CATALOG.filter((k) => k.data).map((k) => [k.name, k.id, k.data!] as const))(
    "a tap on %s sends its sequence once",
    (name, id, data) => {
      const { sendData } = renderToolbar({ keys: [id] });
      const key = screen.getByLabelText(name);
      fireEvent.pointerDown(key);
      fireEvent.pointerUp(key);
      fireEvent.click(key);
      expect(sendData.mock.calls).toEqual([[data]]);
    },
  );

  it("repeats a held key until release, without a trailing tap", () => {
    vi.useFakeTimers();
    const { sendData } = renderToolbar({ keys: ["backspace"] });
    const back = screen.getByLabelText("Backspace");
    fireEvent.pointerDown(back);
    act(() => vi.advanceTimersByTime(HOLD_REPEAT_DELAY_MS - 1));
    expect(sendData).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(1 + 2 * HOLD_REPEAT_INTERVAL_MS));
    expect(sendData).toHaveBeenCalledTimes(3);
    fireEvent.pointerUp(back);
    fireEvent.click(back);
    act(() => vi.advanceTimersByTime(10 * HOLD_REPEAT_INTERVAL_MS));
    expect(sendData).toHaveBeenCalledTimes(3);
    expect(new Set(sendData.mock.calls.map(([d]) => d))).toEqual(new Set(["\x7f"]));
  });

  it("sends nothing when the browser cancels the touch", () => {
    vi.useFakeTimers();
    const { sendData } = renderToolbar({ keys: ["page-up"] });
    const key = screen.getByLabelText("Page up");
    fireEvent.pointerDown(key);
    fireEvent.pointerCancel(key);
    act(() => vi.advanceTimersByTime(HOLD_REPEAT_DELAY_MS * 2));
    expect(sendData).not.toHaveBeenCalled();
  });

  // Every toolbar send bypasses the textarea's beforeinput, so the retained syllable must be gone before the PTY
  // sees the key, or the next Korean keystroke rewrites the stale value into the new line.
  it("drops the retained IME shadow before each out-of-band send", () => {
    vi.useFakeTimers();
    const proxy = document.createElement("textarea");
    proxy.setAttribute("data-keyboard-proxy", "");
    document.body.append(proxy);
    const local = document.createElement("textarea");
    // Asserted inside the mock: an implementation that sent first and
    // cleared afterwards would still pass a check made after the call.
    const seen: Array<{ data: string; local: string; proxy: string }> = [];
    const { sendData } = renderToolbar({ inputEl: local });
    sendData.mockImplementation((data: string) => {
      seen.push({ data, local: local.value, proxy: proxy.value });
      return true;
    });

    for (const label of ["Tab", "Escape", "Ctrl+C interrupt"]) {
      local.value = "ㅎ";
      proxy.value = "ㅎ";
      fireEvent.click(screen.getByLabelText(label));
    }
    local.value = "ㅎ";
    proxy.value = "ㅎ";
    const back = screen.getByLabelText("Backspace");
    fireEvent.pointerDown(back);
    act(() => vi.advanceTimersByTime(HOLD_REPEAT_DELAY_MS));
    fireEvent.pointerUp(back);

    expect(seen.map((s) => s.data)).toEqual(["\t", "\x1b", "\x03", "\x7f"]);
    expect(seen.some((s) => s.local.includes("ㅎ") || s.proxy.includes("ㅎ"))).toBe(false);
    proxy.remove();
  });
});

describe("MobileTerminalToolbar paste", () => {
  it("sends clipboard text as one tmux paste", async () => {
    secureContext(true);
    const item = {
      types: ["text/plain"],
      getType: async () => new Blob(["line 1\nline 2"], { type: "text/plain" }),
    };
    Object.defineProperty(navigator, "clipboard", { value: { read: async () => [item] }, configurable: true });
    const { sendData, sendPaste, onCompose } = renderToolbar({ keyboardOpen: true });

    fireEvent.click(screen.getByLabelText("Paste from clipboard"));
    await waitFor(() => expect(sendPaste).toHaveBeenCalledWith("line 1\nline 2", false));
    expect(sendData).not.toHaveBeenCalled();
    expect(onCompose).not.toHaveBeenCalled();
  });

  it("opens the compose sheet when the clipboard read is refused", async () => {
    secureContext(true);
    Object.defineProperty(navigator, "clipboard", {
      value: { read: async () => Promise.reject(new Error("denied")) },
      configurable: true,
    });
    const { sendPaste, onCompose } = renderToolbar();

    fireEvent.click(screen.getByLabelText("Paste from clipboard"));
    await waitFor(() => expect(onCompose).toHaveBeenCalledTimes(1));
    expect(sendPaste).not.toHaveBeenCalled();
  });

  it("on a plain-HTTP origin pastes through execCommand into the focused input, else opens compose", async () => {
    secureContext(false);
    const editable = document.createElement("textarea");
    document.body.appendChild(editable);
    editable.focus();

    for (const [granted, composes] of [
      [true, 0],
      [false, 1],
    ] as const) {
      const execCommand = vi.fn(() => granted);
      Object.defineProperty(document, "execCommand", { value: execCommand, configurable: true });
      const { sendData, sendPaste, onCompose, unmount } = renderToolbar({ keyboardOpen: true });
      fireEvent.click(screen.getByLabelText("Paste from clipboard"));
      await waitFor(() => expect(execCommand).toHaveBeenCalledWith("paste"));
      // The focused input's own paste handler sends; the toolbar never does.
      expect(sendData).not.toHaveBeenCalled();
      expect(sendPaste).not.toHaveBeenCalled();
      expect(onCompose).toHaveBeenCalledTimes(composes);
      unmount();
    }
    document.body.removeChild(editable);
  });
});

// User story (ported from the live Playwright acp-stories suite): the Ctrl toggle latches the modifier so the next
// keystroke combines with Ctrl.
function CtrlLatchHarness({ sendData }: { sendData: (data: string) => boolean }) {
  const [ctrlActive, setCtrlActive] = useState(false);
  return (
    <MobileTerminalToolbar
      keys={["ctrl", "ctrl-c"]}
      sendData={sendData}
      sendPaste={vi.fn()}
      onCompose={vi.fn()}
      inputElRef={{ current: null }}
      keyboardOpen={false}
      ctrlActive={ctrlActive}
      onCtrlToggle={() => setCtrlActive((v) => !v)}
    />
  );
}

describe("MobileTerminalToolbar Ctrl latch", () => {
  it("tapping Ctrl toggles the latch and Ctrl+C interrupt clears it", () => {
    const sendData = vi.fn(() => true);
    render(<CtrlLatchHarness sendData={sendData} />);
    const ctrl = screen.getByRole("button", { name: "Ctrl" });
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");

    fireEvent.click(ctrl);
    expect(ctrl.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(ctrl);
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");

    fireEvent.click(ctrl);
    fireEvent.click(screen.getByRole("button", { name: "Ctrl+C interrupt" }));
    expect(sendData).toHaveBeenCalledWith("\x03");
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");
  });
});
