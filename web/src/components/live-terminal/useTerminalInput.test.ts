// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook } from "@testing-library/react";
import { useTerminalInput } from "./useTerminalInput";

afterEach(() => {
  cleanup();
  document.body.replaceChildren();
});

describe("useTerminalInput Tab escape", () => {
  it("lets onTabEscape take a plain Tab from the pane's input and App's keyboard proxy before it is sent", () => {
    const input = document.createElement("textarea");
    const proxy = document.createElement("textarea");
    proxy.setAttribute("data-keyboard-proxy", "");
    document.body.append(input, proxy);
    const sendData = vi.fn((_data: string) => true);
    let escape = false;
    const onTabEscape = vi.fn(() => escape);
    const { result } = renderHook(() =>
      useTerminalInput({
        active: true,
        inputRef: { current: input },
        ctrlActiveRef: { current: false },
        clearCtrl: () => {},
        sendData,
        sendPaste: () => true,
        uploadPastedImage: async () => null,
        captureWriteAdmission: () => () => true,
        onTabEscape,
      }),
    );
    const routes = {
      "pane input": (init: KeyboardEventInit) => {
        const e = new KeyboardEvent("keydown", { ...init, cancelable: true });
        result.current.onKeyDown(e);
        return e;
      },
      "keyboard proxy": (init: KeyboardEventInit) => {
        const e = new KeyboardEvent("keydown", { ...init, bubbles: true, cancelable: true });
        proxy.dispatchEvent(e);
        return e;
      },
    };
    const cases: [string, KeyboardEventInit, boolean, string | null][] = [
      ["taken", { key: "Tab" }, true, null],
      ["declined", { key: "Tab" }, false, "\t"],
      ["Shift+Tab is never offered", { key: "Tab", shiftKey: true }, true, "\x1b[Z"],
    ];
    for (const [route, press] of Object.entries(routes)) {
      for (const [name, init, taken, sent] of cases) {
        escape = taken;
        sendData.mockClear();
        onTabEscape.mockClear();
        const e = press(init);
        const label = `${route}: ${name}`;
        expect(e.defaultPrevented, label).toBe(true);
        expect(sendData.mock.calls, label).toEqual(sent === null ? [] : [[sent]]);
        expect(onTabEscape, label).toHaveBeenCalledTimes(init.shiftKey ? 0 : 1);
      }
    }
  });
});
