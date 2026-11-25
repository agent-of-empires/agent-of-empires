import type { KeyboardEvent as ReactKeyboardEvent } from "react";

/** True while an IME composition owns the key event. Safari ends the composition before the Enter that
 *  commits it, leaving `isComposing` false, so keyCode 229 is the only marker left on that event. */
export function isImeComposing(e: KeyboardEvent | ReactKeyboardEvent): boolean {
  const native = "nativeEvent" in e ? e.nativeEvent : e;
  return native.isComposing || native.keyCode === 229;
}
