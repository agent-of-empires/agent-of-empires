// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { HIDDEN_INPUT_SENTINEL as S } from "./hiddenInputDiff";
import {
  bindHiddenInput,
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  invalidateRetainedImeContext,
  registerMobileKeyboardProxyReceiver,
  type MobileKeyboardProxyInput,
} from "./mobileKeyboardProxy";

afterEach(() => {
  clearMobileKeyboardProxyInput();
  document.body.innerHTML = "";
});

const edit = (data: string, deleted = 0): MobileKeyboardProxyInput => ({ inputType: "edit", deleted, data });

function bound(accepted = true) {
  const ta = document.createElement("textarea");
  document.body.append(ta);
  const deliver = vi.fn<(input: MobileKeyboardProxyInput) => boolean>(() => accepted);
  bindHiddenInput(ta, deliver, "test");
  /** Applies a native edit the way the browser does: the value changes, then `input` fires. */
  const apply = (value: string, init: InputEventInit = {}) => {
    ta.value = value;
    ta.dispatchEvent(new InputEvent("input", { bubbles: true, ...init }));
  };
  const beforeInput = (init: InputEventInit) => {
    const ev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, ...init });
    ta.dispatchEvent(ev);
    return ev;
  };
  return { ta, deliver, apply, beforeInput };
}

describe("bindHiddenInput", () => {
  it("starts with the sentinel and sends each edit as a diff", () => {
    const { ta, deliver, apply } = bound();
    expect(ta.value).toBe(S);
    apply(S + "thi", { inputType: "insertText" });
    apply(S + "this is", { inputType: "insertReplacementText" });
    apply(S + "This is", { inputType: "insertReplacementText" });
    expect(deliver.mock.calls.map(([i]) => i)).toEqual([edit("thi"), edit("s is"), edit("This is", 7)]);
  });

  it("refills the sentinel a delete reached and keeps the caret at the end", () => {
    const { ta, deliver, apply } = bound();
    apply(S.slice(1), { inputType: "deleteContentBackward" });
    expect(deliver).toHaveBeenCalledWith(edit("", 1));
    expect([ta.value, ta.selectionStart]).toEqual([S, S.length]);
  });

  it("waits for compositionend to send a composition", () => {
    const { ta, deliver, apply } = bound();
    ta.dispatchEvent(new CompositionEvent("compositionstart"));
    apply(S + "n", { isComposing: true });
    apply(S + "android", { isComposing: true });
    expect(deliver).not.toHaveBeenCalled();
    ta.dispatchEvent(new CompositionEvent("compositionend", { data: "android" }));
    expect(deliver.mock.calls).toEqual([[edit("android")]]);
  });

  it("resets a refused edit so the textarea never shadows unsent text", () => {
    const { ta, deliver, apply } = bound(false);
    apply(S + "c", { inputType: "insertText" });
    expect(deliver).toHaveBeenCalledWith(edit("c"));
    expect(ta.value).toBe(S);
    apply(S + "ㅎ", { inputType: "insertText" });
    expect(deliver).toHaveBeenLastCalledWith(edit("ㅎ"));
  });

  it("cancels a line break, flushes pending text first, and resets", () => {
    const { ta, deliver, beforeInput } = bound();
    ta.value = S + "ls";
    const ev = beforeInput({ inputType: "insertParagraph" });
    expect(ev.defaultPrevented).toBe(true);
    expect(deliver.mock.calls).toEqual([[edit("ls")], [{ inputType: "insertParagraph" }]]);
    expect(ta.value).toBe(S);
  });

  it("cancels a paste and reports it", () => {
    const { deliver, beforeInput } = bound();
    const ev = beforeInput({ inputType: "insertFromPaste", data: "a\nb" });
    expect(ev.defaultPrevented).toBe(true);
    expect(deliver).toHaveBeenCalledWith({ inputType: "insertFromPaste", data: "a\nb" });
  });

  it("returns a moved caret to the end before a key edits", () => {
    const { ta } = bound();
    ta.value = S + "hello";
    ta.setSelectionRange(S.length + 2, S.length + 2);
    ta.dispatchEvent(new KeyboardEvent("keydown", { key: "x" }));
    expect(ta.selectionStart).toBe(ta.value.length);
  });
});

describe("mobile keyboard proxy", () => {
  function proxy(value: string) {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const ta = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    ta.value = value;
    return ta;
  }

  it("rejects input past the queue bound", () => {
    for (let i = 0; i < 128; i++) expect(deliverMobileKeyboardProxyInput(edit(`x${i}`))).toBe(true);
    expect(deliverMobileKeyboardProxyInput(edit("over"))).toBe(false);
  });

  it("drops queued input and the proxy text at a session boundary", () => {
    const ta = proxy(S + "old");
    deliverMobileKeyboardProxyInput(edit("old"));
    clearMobileKeyboardProxyInput();
    const receive = vi.fn(() => true);
    registerMobileKeyboardProxyReceiver(receive);
    expect(receive).not.toHaveBeenCalled();
    expect(ta.value).toBe(S);
  });

  it.each([
    ["keeps the proxy text when every replayed edit is accepted", [true, true], S + "cㅎ"],
    ["resets the proxy when a replayed edit is refused", [false, true], S],
  ])("%s", (_name, results, value) => {
    const ta = proxy(S + "cㅎ");
    deliverMobileKeyboardProxyInput(edit("c"));
    deliverMobileKeyboardProxyInput(edit("ㅎ"));
    const receive = vi.fn();
    for (const r of results) receive.mockReturnValueOnce(r);
    const unregister = registerMobileKeyboardProxyReceiver(receive);
    expect(receive.mock.calls).toEqual([[edit("c")], [edit("ㅎ")]]);
    expect(ta.value).toBe(value);
    unregister();
  });

  it("keeps the current receiver when an older cleanup runs", () => {
    const first = vi.fn(() => true);
    const stop1 = registerMobileKeyboardProxyReceiver(first);
    const second = vi.fn(() => true);
    const stop2 = registerMobileKeyboardProxyReceiver(second);
    stop1();
    deliverMobileKeyboardProxyInput(edit("x"));
    expect(second).toHaveBeenCalledWith(edit("x"));
    expect(first).not.toHaveBeenCalled();
    stop2();
  });
});

describe("invalidateRetainedImeContext", () => {
  it.each([true, false])("resets the proxy, and the given input when passed=%s", (passLocal) => {
    document.body.innerHTML = "<textarea data-keyboard-proxy></textarea>";
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]")!;
    proxy.value = S + "ㅎ";
    const local = document.createElement("textarea");
    local.value = S + "ㅎ";
    invalidateRetainedImeContext(passLocal ? local : undefined);
    expect([local.value, proxy.value]).toEqual([passLocal ? S : S + "ㅎ", S]);
  });

  it("makes the next edit diff against the reset value", () => {
    const deliver = vi.fn(() => true);
    const ta = document.createElement("textarea");
    bindHiddenInput(ta, deliver, "test");
    ta.value = S + "ㅎ";
    ta.dispatchEvent(new InputEvent("input"));
    invalidateRetainedImeContext(ta);
    ta.value = S + "a";
    ta.dispatchEvent(new InputEvent("input"));
    expect(deliver).toHaveBeenLastCalledWith(edit("a"));
  });

  it("tolerates a missing proxy", () => {
    expect(() => invalidateRetainedImeContext(null)).not.toThrow();
  });
});
