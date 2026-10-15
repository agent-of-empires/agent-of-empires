// iOS 26 installed web apps report `100dvh` short by the top safe-area inset, leaving a dead band at the bottom;
// only `100lvh` spans the screen. On iOS standalone this marks <html data-ios-standalone>, plus data-editing while a
// control that raises the soft keyboard has focus, so index.css uses `100lvh` only with no keyboard and leaves the
// `100dvh` keyboard shrink alone. Every other platform, Android included, keeps `100dvh` untouched.

import { isIOS, isStandalone } from "./platform";

const NON_TEXT_INPUTS = ["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"];

export function raisesKeyboard(el: Element | null): boolean {
  if (el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) return true;
  if (el instanceof HTMLInputElement) return !NON_TEXT_INPUTS.includes(el.type);
  return el instanceof HTMLElement && el.isContentEditable === true;
}

export function installEditingFocus(doc: Document = document, iosStandalone = isIOS() && isStandalone()): () => void {
  if (!iosStandalone) return () => {};
  const html = doc.documentElement;
  html.setAttribute("data-ios-standalone", "");
  const sync = () => html.toggleAttribute("data-editing", raisesKeyboard(doc.activeElement));
  // Between focusout and the next focusin the active element is <body>; settle after both.
  const syncSoon = () => queueMicrotask(sync);
  doc.addEventListener("focusin", sync);
  doc.addEventListener("focusout", syncSoon);
  sync();
  return () => {
    html.removeAttribute("data-ios-standalone");
    doc.removeEventListener("focusin", sync);
    doc.removeEventListener("focusout", syncSoon);
  };
}
