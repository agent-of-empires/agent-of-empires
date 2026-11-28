import { useAuiState } from "@assistant-ui/react";
import { ChevronDown, Files } from "lucide-react";
import { useId, useRef, useState } from "react";

import { DropdownMenu, MenuItem } from "../ContextMenu";
import { moveMenuFocus } from "../menuFocus";
import { useOutsideDismiss } from "../useOutsideDismiss";
import { CopyIconState, CopyMarkdownButton } from "./CopyMarkdownButton";
import { messageToMarkdown, type MessagePart } from "./messageMarkdown";
import { COMPACT_COPY_BUTTON_CLASS, useCopy } from "./useCopy";

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
  const [openAt, setOpenAt] = useState<"first" | "last">("first");
  const menuId = useId();
  useOutsideDismiss(open, [ref], () => setOpen(false));

  // The menu-button pattern: focus moves into the menu when it opens, arrows move within it, and Escape or a
  // selection puts focus back on the trigger. A click elsewhere dismisses without taking focus back.
  const close = () => {
    setOpen(false);
    triggerRef.current?.focus();
  };
  const onTriggerKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      setOpenAt(e.key === "ArrowDown" ? "first" : "last");
      setOpen(true);
    } else if (e.key === "Escape" && open) {
      close();
    }
  };
  const onMenuKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "Tab") {
      setOpen(false);
    } else {
      moveMenuFocus(e);
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
                setOpenAt("first");
                setOpen((o) => !o);
              }}
              onKeyDown={onTriggerKeyDown}
              className={`${COMPACT_COPY_BUTTON_CLASS} rounded-l-none border-l border-surface-800`}
            >
              <ChevronDown className="h-3 w-3" />
            </button>
            {open && (
              <DropdownMenu
                id={menuId}
                labelledBy={`${menuId}-trigger`}
                initialFocus={openAt}
                onKeyDown={onMenuKeyDown}
                className="absolute right-0 top-full z-20 mt-1 min-w-[11rem]"
              >
                <MenuItem
                  role="menuitem"
                  tabIndex={-1}
                  disabled={!hasText}
                  onClick={() => {
                    close();
                    copyMarkdown(false);
                  }}
                >
                  Copy text
                </MenuItem>
                <MenuItem
                  role="menuitem"
                  tabIndex={-1}
                  onClick={() => {
                    close();
                    copyMarkdown(true);
                  }}
                >
                  Copy with tool calls
                </MenuItem>
              </DropdownMenu>
            )}
          </>
        )}
      </div>
    </div>
  );
}
