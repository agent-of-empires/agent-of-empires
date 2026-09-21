// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  forwardTerminalBeforeInput,
  invalidateRetainedImeContext,
  registerMobileKeyboardProxyReceiver,
} from "./mobileKeyboardProxy";

afterEach(clearMobileKeyboardProxyInput);

function beforeInput(target: HTMLTextAreaElement, init: InputEventInit, delivered = true) {
  const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init });
  const deliver = vi.fn(() => delivered);
  target.addEventListener("beforeinput", (e) => forwardTerminalBeforeInput(e as InputEvent, deliver), { once: true });
  target.dispatchEvent(ev);
  return { ev, deliver };
}

describe("forwardTerminalBeforeInput", () => {
  it.each<[string, InputEventInit, string, boolean, boolean, string]>([
    ["forwards an accepted insert into the textarea", { inputType: "insertText", data: "ㅎ" }, "", true, false, ""],
    ["forwards an accepted delete", { inputType: "deleteContentBackward" }, "ㅎ", true, false, "ㅎ"],
    ["swallows a line break and drops the IME context", { inputType: "insertLineBreak" }, "한국어", true, true, ""],
    ["swallows a paste", { inputType: "insertFromPaste", data: "a\nb" }, "", true, true, ""],
    ["cancels a refused insert", { inputType: "insertText", data: "c" }, "한", false, true, ""],
    ["cancels a refused delete and drops the text", { inputType: "deleteContentBackward" }, "그", false, true, ""],
  ])("%s", (_name, init, before, accepted, prevented, after) => {
    const ta = document.createElement("textarea");
    ta.value = before;
    const { ev, deliver } = beforeInput(ta, init, accepted);
    expect(deliver).toHaveBeenCalledWith({ inputType: init.inputType, data: init.data ?? null, isComposing: false });
    expect(ev.defaultPrevented).toBe(prevented);
    expect(ta.value).toBe(after);
  });

  it("ignores other input types", () => {
    const { ev, deliver } = beforeInput(document.createElement("textarea"), {
      inputType: "insertReplacementText",
      data: "x",
    });
    expect(deliver).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it.each<[InputEventInit, boolean]>([
    [{ inputType: "insertLineBreak" }, true],
    [{ inputType: "insertText", data: "c" }, false],
  ])("tolerates a non-textarea target (%o)", (init, accepted) => {
    const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init });
    Object.defineProperty(ev, "target", { value: document.createElement("div") });
    expect(() => forwardTerminalBeforeInput(ev, () => accepted)).not.toThrow();
    expect(ev.defaultPrevented).toBe(true);
  });
});

describe("mobile keyboard proxy", () => {
  it("delivers input buffered while a session is mounting", () => {
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "first", isComposing: false });
    const receive = vi.fn();
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalledWith({ inputType: "insertText", data: "first", isComposing: false });
  });

  it("rejects input past the queue bound", () => {
    for (let i = 0; i < 128; i++) {
      const ok = deliverMobileKeyboardProxyInput({ inputType: "insertText", data: `x${i}`, isComposing: false });
      expect(ok).toBe(true);
    }
    expect(deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "over", isComposing: false })).toBe(false);
  });

  it("drops queued input at a session boundary", () => {
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "old", isComposing: false });
    clearMobileKeyboardProxyInput();
    const receive = vi.fn();
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).not.toHaveBeenCalled();
  });

  it("clears the proxy when a drained queued edit is refused", () => {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = "ㅎ";
    expect(proxy.value).toBe("ㅎ");
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "가", isComposing: false });

    const receive = vi.fn(() => false);
    const unregister = registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalledWith({ inputType: "insertText", data: "가", isComposing: false });
    expect(proxy.value).toBe("");
    unregister();
    document.body.innerHTML = "";
  });

  it("keeps the proxy content when drained edits are accepted", () => {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = "ㅎ";
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "가", isComposing: false });
    const receive = vi.fn(() => true);
    const unregister = registerMobileKeyboardProxyReceiver(receive);
    expect(receive).toHaveBeenCalled();
    expect(proxy.value).toBe("ㅎ");
    unregister();
    document.body.innerHTML = "";
  });

  it("keeps the current receiver when an older cleanup runs", () => {
    const first = vi.fn(() => true);
    const stop1 = registerMobileKeyboardProxyReceiver(first);
    const second = vi.fn(() => true);
    const stop2 = registerMobileKeyboardProxyReceiver(second);
    stop1();
    deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "x", isComposing: false });
    expect(second).toHaveBeenCalledWith({ inputType: "insertText", data: "x", isComposing: false });
    expect(first).not.toHaveBeenCalledWith({ inputType: "insertText", data: "x", isComposing: false });
    stop2();
  });
});

describe("invalidateRetainedImeContext", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  it.each([true, false])("clears the proxy, and the given input when passed=%s", (passLocal) => {
    document.body.innerHTML = "<textarea data-keyboard-proxy>ㅎ</textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = "ㅎ";
    const local = document.createElement("textarea");
    local.value = "ㅎ";
    invalidateRetainedImeContext(passLocal ? local : undefined);
    expect([local.value, proxy.value]).toEqual([passLocal ? "" : "ㅎ", ""]);
  });

  it("tolerates a missing proxy", () => {
    expect(() => invalidateRetainedImeContext(null)).not.toThrow();
  });
});
