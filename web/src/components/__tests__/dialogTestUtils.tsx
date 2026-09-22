import { expect } from "vitest";

/** Mounts a dialog while a trigger holds focus and asserts focus returns to it on unmount. */
export function expectRestoresFocus(mount: () => () => void) {
  const trigger = document.createElement("button");
  document.body.appendChild(trigger);
  trigger.focus();
  const unmount = mount();
  expect(document.activeElement).not.toBe(trigger);
  unmount();
  expect(document.activeElement).toBe(trigger);
  trigger.remove();
}
