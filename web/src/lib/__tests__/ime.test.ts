import { describe, expect, it } from "vitest";

import { isImeComposing } from "../ime";

describe("isImeComposing", () => {
  const native = (isComposing: boolean, keyCode: number) => ({ key: "Enter", isComposing, keyCode });

  it.each([
    ["native isComposing", native(true, 13), true],
    ["native keyCode 229", native(false, 229), true],
    ["native plain Enter", native(false, 13), false],
    ["react isComposing", { nativeEvent: native(true, 13) }, true],
    ["react keyCode 229", { nativeEvent: native(false, 229) }, true],
    ["react plain Enter", { nativeEvent: native(false, 13) }, false],
  ])("%s", (_name, event, expected) => {
    expect(isImeComposing(event as unknown as KeyboardEvent)).toBe(expected);
  });
});
