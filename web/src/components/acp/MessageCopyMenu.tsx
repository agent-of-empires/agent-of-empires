import { useAuiState } from "@assistant-ui/react";
import { ChevronDown, Files } from "lucide-react";
import { useCallback, useId, useRef, useState } from "react";

import { useOutsideDismiss } from "../useOutsideDismiss";
import { CopyIconState, CopyMarkdownButton } from "./CopyMarkdownButton";
import { messageToMarkdown, type MessagePart } from "./messageMarkdown";
import { COMPACT_COPY_BUTTON_CLASS, useCopy } from "./useCopy";

const MENU_ITEM_CLASS = "block w-full px-3 py-1.5 text-left text-xs text-text-secondary hover:bg-surface-700";

/**
 * The message's footer row. The icon copies the whole message's text; with tool calls present, the chevron
 * offers them too. When the message ends in a text part, that part's own button shares the row.
 */
export function MessageCopyMenu() {
  const parts = useAuiState((s) => s.message.content) as readonly MessagePart[];
  const [copied, copy] = useCopy();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const openAt = useRef<"first" | "last">("first");
  const menuId = useId();
  useOutsideDismiss(open, [ref], () => setOpen(false));

  // The menu-button pattern: focus moves into the menu when it opens, arrows move within it, and Escape or a
  // selection puts focus back on the trigger. A click elsewhere dismisses without taking focus back.
  const items = () => [...(menuRef.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? [])];
  const focusItem = (index: number) => {
    const list = items();
    list[(index + list.length) % list.length]?.focus();
  };
  const close = () => {
    setOpen(false);
    triggerRef.current?.focus();
  };
  // Stable, so it runs once when the menu mounts and not on every render while it is open.
  const mountMenu = useCallback((el: HTMLDivElement | null) => {
    menuRef.current = el;
    const list = el?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)');
    if (list?.length) list[openAt.current === "last" ? list.length - 1 : 0]?.focus();
  }, []);
  const onTriggerKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      openAt.current = e.key === "ArrowDown" ? "first" : "last";
      setOpen(true);
    } else if (e.key === "Escape" && open) {
      close();
    }
  };
  const onMenuKeyDown = (e: React.KeyboardEvent) => {
    const at = items().indexOf(document.activeElement as HTMLElement);
    const move: Record<string, number> = { ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: -1 };
    if (e.key in move) {
      e.preventDefault();
      focusItem(move[e.key]!);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "Tab") {
      setOpen(false);
    }
  };

  const hasText = parts.some((p) => p.type === "text" && p.text?.trim());
  const hasTools = parts.some((p) => p.type === "tool-call");
  if (!hasText && !hasTools) return null;
  // Serialized on click: group payloads need parsing, which a render should not pay for.
  const copyMarkdown = (includeTools: boolean) => void copy(messageToMarkdown(parts, { includeTools }));
  const label = copied ? "Copied" : "Copy message as markdown";
  // A lone part is the whole message, so there is nothing separate to copy.
  const multiPart = parts.length > 1;
  const last = parts[parts.length - 1];
  const lastText = multiPart && last?.type === "text" && last.text?.trim() ? last.text : null;

  return (
    // A footer row, so it never covers the message's own content or its text parts' buttons.
    <div className="acp-copy-reveal flex items-center justify-end gap-1" data-copied={copied} data-open={open}>
      {lastText && <CopyMarkdownButton source={lastText} label="Copy text as markdown" compact />}
      <div ref={ref} className="relative flex">
        <button
          type="button"
          aria-label={label}
          title={label}
          disabled={!hasText}
          onClick={() => copyMarkdown(false)}
          className={`${COMPACT_COPY_BUTTON_CLASS} ${hasTools ? "rounded-r-none" : ""} disabled:opacity-40`}
        >
          <CopyIconState copied={copied} Icon={multiPart ? Files : undefined} />
        </button>
        {hasTools && (
          <>
            <button
              ref={triggerRef}
              type="button"
              id={`${menuId}-trigger`}
              aria-label="More copy options"
              aria-haspopup="menu"
              aria-expanded={open}
              aria-controls={open ? menuId : undefined}
              onClick={() => {
                openAt.current = "first";
                setOpen((o) => !o);
              }}
              onKeyDown={onTriggerKeyDown}
              className={`${COMPACT_COPY_BUTTON_CLASS} rounded-l-none border-l border-surface-800`}
            >
              <ChevronDown className="h-3 w-3" />
            </button>
            {open && (
              <div
                ref={mountMenu}
                id={menuId}
                role="menu"
                aria-labelledby={`${menuId}-trigger`}
                onKeyDown={onMenuKeyDown}
                className="absolute right-0 top-full z-20 mt-1 min-w-[10rem] rounded-md border border-surface-700/50 bg-surface-800 py-1 shadow-xl"
              >
                <button
                  type="button"
                  role="menuitem"
                  tabIndex={-1}
                  disabled={!hasText}
                  className={`${MENU_ITEM_CLASS} disabled:opacity-40`}
                  onClick={() => {
                    close();
                    copyMarkdown(false);
                  }}
                >
                  Copy text
                </button>
                <button
                  type="button"
                  role="menuitem"
                  tabIndex={-1}
                  className={MENU_ITEM_CLASS}
                  onClick={() => {
                    close();
                    copyMarkdown(true);
                  }}
                >
                  Copy with tool calls
                </button>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
