// Pure keyboard decisions, layout, textarea writes, and attachment helpers for the Composer.

import type { Unstable_TriggerItem } from "@assistant-ui/core";

import type { PromptAttachmentKind, PromptCapabilities } from "../../lib/acpTypes";
import { replaceSlashCommand } from "./slashCompletion";

interface KeyInfo {
  key: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  metaKey: boolean;
  isComposing: boolean;
}

/** `send` routes Enter through our queue path, which the primitive blocks while
 *  a turn runs; `default` leaves the primitive's keymap alone. Touch-primary
 *  Enter is always a newline (`unstable_insertNewlineOnTouchEnter`). */
export type EnterAction = "send" | "default";

export function decideEnterAction(event: KeyInfo, ctx: { isMobile: boolean; turnActive: boolean }): EnterAction {
  if (event.key !== "Enter" || event.isComposing) return "default";
  if (event.shiftKey || event.ctrlKey || event.metaKey) return "default";
  return !ctx.isMobile && ctx.turnActive ? "send" : "default";
}

export type ArrowRecallAction = "older" | "newer" | "default";

/** Shell-history queue recall. ArrowUp enters only from the caret origin with a
 *  non-empty queue, so multi-line caret movement is never hijacked. */
export function decideArrowRecall(
  event: KeyInfo & { altKey: boolean },
  ctx: { caretAtStart: boolean; browsing: boolean; queueLen: number },
): ArrowRecallAction {
  if (event.isComposing) return "default";
  if (event.shiftKey || event.ctrlKey || event.metaKey || event.altKey) return "default";
  if (event.key === "ArrowUp" && (ctx.browsing || (ctx.caretAtStart && ctx.queueLen > 0))) return "older";
  if (event.key === "ArrowDown" && ctx.browsing) return "newer";
  return "default";
}

export type BeforeInputAction = "newline" | "default";

/** Android soft keyboards send Enter as `beforeinput` line breaks with no usable
 *  keydown; on mobile those become a manual newline so the primitive cannot send. */
export function decideBeforeInputAction(
  inputType: string,
  isComposing: boolean,
  ctx: { isMobile: boolean },
): BeforeInputAction {
  if (!ctx.isMobile || isComposing) return "default";
  return inputType === "insertLineBreak" || inputType === "insertParagraph" ? "newline" : "default";
}

/** iOS keyboard accessory bar height; on an installed PWA it floats over Send. */
export const IOS_ACCESSORY_BAR_PX = 44;

export function composerWrapperLayout(opts: { keyboardOpen: boolean; accessoryBarPx?: number }): {
  className: string;
  style: React.CSSProperties | undefined;
} {
  if (!opts.keyboardOpen) {
    return { className: "border-t border-surface-800 bg-surface-900 px-4 pt-3 pb-3", style: undefined };
  }
  const clearance = opts.accessoryBarPx ?? 0;
  return {
    className: "border-t border-surface-800 bg-surface-900 px-4 pt-3 pb-0",
    style: clearance > 0 ? { paddingBottom: clearance } : undefined,
  };
}

/** Auto-grow the textarea up to ~6 lines. */
export function fitTextarea(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
}

/** Replace the value the way a keystroke would. The native setter bypasses React's
 *  patched one, the caret is set before dispatch because the primitive reads it in
 *  the same onChange, and the InputEvent carries the `inputType`/`data` the
 *  trigger popover keys off. */
function writeComposerValue(ta: HTMLTextAreaElement, next: string, caret: number, inputType: string, data?: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
  setter?.call(ta, next);
  ta.focus();
  ta.setSelectionRange(caret, caret);
  ta.dispatchEvent(new InputEvent("input", { bubbles: true, inputType, ...(data === undefined ? {} : { data }) }));
}

type TextareaRef = React.RefObject<HTMLTextAreaElement | null>;

/** Replace the `/token` at the caret with the picked command. Written through the
 *  DOM, not `setText`, so assistant-ui's trigger cursor moves with the text and the
 *  popover closes instead of swallowing the next Enter. */
export function insertSlashCommand(ref: TextareaRef, item: Unstable_TriggerItem) {
  const ta = ref.current;
  if (!ta) return;
  const caret = ta.selectionStart ?? ta.value.length;
  const { text, cursor } = replaceSlashCommand(ta.value, caret, ta.selectionEnd ?? caret, item.id);
  writeComposerValue(ta, text, cursor, "insertText", `/${item.id}`);
}

export function insertNewlineAtCaret(ref: TextareaRef): void {
  const ta = ref.current;
  if (!ta) return;
  const start = ta.selectionStart ?? ta.value.length;
  const before = ta.value.slice(0, start);
  writeComposerValue(
    ta,
    `${before}\n${ta.value.slice(ta.selectionEnd ?? start)}`,
    before.length + 1,
    "insertLineBreak",
  );
}

/** Insert a trigger character at the caret, padding with a space mid-word so detection fires. */
export function insertAtCaret(ref: TextareaRef, text: string) {
  const ta = ref.current;
  if (!ta) return;
  const start = ta.selectionStart ?? ta.value.length;
  const end = ta.selectionEnd ?? start;
  const before = ta.value.slice(0, start);
  const needsSpace = before.length > 0 && !/[\s\n\t]$/.test(before) ? " " : "";
  const next = before + needsSpace + text + ta.value.slice(end);
  writeComposerValue(ta, next, before.length + needsSpace.length + text.length, "insertText", text);
}

export function insertRawTextAtCaret(ta: HTMLTextAreaElement, text: string, replaceSelection: boolean) {
  const start = ta.selectionStart ?? ta.value.length;
  const end = replaceSelection ? (ta.selectionEnd ?? start) : start;
  writeComposerValue(
    ta,
    ta.value.slice(0, start) + text + ta.value.slice(end),
    start + text.length,
    "insertText",
    text,
  );
  fitTextarea(ta);
}

/** Client-side mirror of the server's per-prompt attachment cap. */
export const MAX_ATTACHMENTS = 8;

export function mimeToKind(mime: string): PromptAttachmentKind {
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  return "resource";
}

export function kindSupported(kind: PromptAttachmentKind, caps: PromptCapabilities | null): boolean {
  if (!caps) return false;
  if (kind === "image") return caps.image;
  if (kind === "audio") return caps.audio;
  return caps.embeddedContext;
}

/** File picker `accept`, narrowed to the kinds the agent takes. */
export function acceptForCaps(caps: PromptCapabilities | null): string {
  const parts: string[] = [];
  if (caps?.image) parts.push("image/*");
  if (caps?.audio) parts.push("audio/*");
  if (caps?.embeddedContext) parts.push(".txt,.md,.json,.pdf");
  return parts.join(",");
}

/** Standard base64 of a File, without the `data:` prefix. */
export function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.onload = () => {
      const result = typeof reader.result === "string" ? reader.result : "";
      resolve(result.slice(result.indexOf(",") + 1));
    };
    reader.readAsDataURL(file);
  });
}
