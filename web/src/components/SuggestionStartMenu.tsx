import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { fetchSuggestionProjects } from "../lib/api";
import type { SuggestionProject } from "../lib/types";
import { useOutsideDismiss } from "./useOutsideDismiss";

export const MENU_ITEM =
  "flex min-h-11 w-full flex-col justify-center px-3 py-2 text-left text-xs text-text-secondary hover:bg-surface-700 focus-visible:bg-surface-700 focus-visible:outline-none disabled:opacity-50 md:min-h-0 md:py-1.5";
export const CHEVRON_BUTTON =
  "relative inline-flex h-8 items-center justify-center gap-1 rounded-md border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700/50 disabled:opacity-50 after:absolute after:left-1/2 after:top-1/2 after:h-11 after:w-11 after:-translate-x-1/2 after:-translate-y-1/2 after:content-[''] md:after:hidden";

const ENABLED_ITEMS = '[role="menuitem"]:not(:disabled)';

/**
 * A popover on desktop and a bottom sheet on a phone. Opening focuses the first item, arrow keys
 * move between items, and closing by Esc or by picking returns focus to the trigger button.
 */
export function MenuPopover({
  label,
  align = "right",
  trigger,
  children,
}: {
  label: string;
  align?: "left" | "right";
  trigger: (props: { open: boolean; toggle: () => void }) => ReactNode;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const box = useRef<HTMLDivElement>(null);
  useOutsideDismiss(open, [box], () => setOpen(false));
  const close = (refocus = true) => {
    setOpen(false);
    if (refocus) document.querySelector<HTMLElement>(`[data-menu="${id}"] [aria-haspopup]`)?.focus();
  };
  const menu = useRef<HTMLDivElement>(null);
  // Every render, since items can arrive after opening (Start in… loads its Projects then).
  useEffect(() => {
    const el = menu.current;
    if (el && !el.contains(document.activeElement)) el.querySelector<HTMLElement>(ENABLED_ITEMS)?.focus();
  });
  const onMenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Tab") return close(false);
    const items = [...e.currentTarget.querySelectorAll<HTMLElement>(ENABLED_ITEMS)];
    const at = items.indexOf(document.activeElement as HTMLElement);
    const next: Record<string, number> = {
      ArrowDown: (at + 1) % items.length,
      ArrowUp: (at - 1 + items.length) % items.length,
      Home: 0,
      End: items.length - 1,
    };
    if (!(e.key in next)) return;
    e.preventDefault();
    items[next[e.key]!]?.focus();
  };
  return (
    <div
      ref={box}
      data-menu={id}
      className="relative"
      onKeyDown={(e) => {
        if (e.key === "Escape" && open) close();
      }}
    >
      {trigger({ open, toggle: () => setOpen((v) => !v) })}
      {open && (
        <>
          <div aria-hidden="true" className="fixed inset-0 z-30 bg-black/40 md:hidden" onClick={() => setOpen(false)} />
          <div
            ref={menu}
            role="menu"
            aria-label={label}
            onKeyDown={onMenuKey}
            className={`fixed inset-x-0 bottom-0 z-40 max-h-[70vh] overflow-y-auto rounded-t-xl border border-surface-700 bg-surface-800 py-2 shadow-lg md:absolute md:inset-x-auto md:bottom-auto ${align === "right" ? "md:right-0" : "md:left-0"} md:mt-1 md:max-h-72 md:w-[270px] md:rounded-md md:py-1`}
          >
            {children(close)}
          </div>
        </>
      )}
    </div>
  );
}

export interface StartMenuProps {
  /** "agent-of-empires · Claude Code · Structured view · worktree" */
  uses?: string;
  disabled: boolean;
  providerName: string;
  sourceUrl: string;
  kindLabel?: string;
  onStartWithChanges: () => void;
  onCopyPrompt: () => void;
  onEditPreset?: () => void;
}

/** The ▾ next to Start. ↓ or Enter on the button opens it. */
export function StartMenu({
  uses,
  disabled,
  providerName,
  sourceUrl,
  kindLabel,
  onStartWithChanges,
  onCopyPrompt,
  onEditPreset,
}: StartMenuProps) {
  return (
    <MenuPopover
      label="Start options"
      trigger={({ open, toggle }) => (
        <button
          type="button"
          aria-label="Start options"
          title="Start options"
          aria-haspopup="menu"
          aria-expanded={open}
          onClick={toggle}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" && !open) {
              e.preventDefault();
              toggle();
            }
          }}
          className={`${CHEVRON_BUTTON} w-8`}
        >
          <ChevronDown className="h-4 w-4" />
        </button>
      )}
    >
      {(close) => (
        <>
          {uses && (
            <div className="border-b border-surface-700/60 px-3 pb-2 pt-1 text-[11px] text-text-dim">
              <div>Start uses</div>
              <div className="mt-0.5 text-text-secondary">{uses}</div>
            </div>
          )}
          <button
            type="button"
            role="menuitem"
            disabled={disabled}
            className={MENU_ITEM}
            onClick={() => {
              close();
              onStartWithChanges();
            }}
          >
            Start with changes
            <span className="block text-[11px] text-text-dim">Opens the New session dialog, prefilled</span>
          </button>
          <button
            type="button"
            role="menuitem"
            className={MENU_ITEM}
            onClick={() => {
              close();
              onCopyPrompt();
            }}
          >
            Copy prompt
          </button>
          {/^https?:\/\//i.test(sourceUrl) && (
            <a
              role="menuitem"
              href={sourceUrl}
              target="_blank"
              rel="noopener noreferrer"
              className={MENU_ITEM}
              onClick={() => close()}
            >
              Open in {providerName} ↗
            </a>
          )}
          {onEditPreset && kindLabel && (
            <button
              type="button"
              role="menuitem"
              className={MENU_ITEM}
              onClick={() => {
                close();
                onEditPreset();
              }}
            >
              Edit the {kindLabel} preset
              <span className="block text-[11px] text-text-dim">Settings › Suggestions</span>
            </button>
          )}
        </>
      )}
    </MenuPopover>
  );
}

/** "Start in… ▾" for a repo with no Project yet: picking one links the repo and starts. */
export function StartInMenu({
  disabled,
  title,
  onPick,
}: {
  disabled: boolean;
  title?: string;
  onPick: (path: string) => void;
}) {
  const [projects, setProjects] = useState<SuggestionProject[] | null>(null);
  const [failed, setFailed] = useState(false);
  const loaded = useRef(false);
  const load = () => {
    if (loaded.current) return;
    loaded.current = true;
    void fetchSuggestionProjects().then((list) => {
      if (list) setProjects(list);
      else {
        setFailed(true);
        loaded.current = false;
      }
    });
  };
  return (
    <MenuPopover
      label="Start in a Project"
      trigger={({ open, toggle }) => (
        <StartInTrigger
          open={open}
          disabled={disabled}
          title={title}
          onToggle={() => {
            if (!open) load();
            toggle();
          }}
        />
      )}
    >
      {(close) =>
        projects === null ? (
          <p className="px-3 py-2 text-xs text-text-dim" role="status">
            {failed ? "Could not load Projects." : "Loading Projects…"}
          </p>
        ) : projects.length === 0 ? (
          <p className="px-3 py-2 text-xs text-text-dim">No Projects yet. Add one from the dashboard.</p>
        ) : (
          projects.map((p) => (
            <button
              key={p.path}
              type="button"
              role="menuitem"
              title={p.path}
              className={MENU_ITEM}
              onClick={() => {
                close();
                onPick(p.path);
              }}
            >
              {p.name}
            </button>
          ))
        )
      }
    </MenuPopover>
  );
}

function StartInTrigger({
  open,
  disabled,
  title,
  onToggle,
}: {
  open: boolean;
  disabled: boolean;
  title?: string;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      aria-haspopup="menu"
      aria-expanded={open}
      disabled={disabled}
      title={title}
      onClick={onToggle}
      className={`${CHEVRON_BUTTON} px-3 text-xs font-medium`}
    >
      Start in…
      <ChevronDown className="h-3.5 w-3.5" />
    </button>
  );
}
