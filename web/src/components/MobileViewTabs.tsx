import { Ellipsis, FileDiff, FolderTree, Sparkles, SquareTerminal, type LucideIcon } from "lucide-react";

import type { RightPanelView } from "../lib/rightPanelView";

const TABS: readonly { view: RightPanelView; label: string; Icon: LucideIcon }[] = [
  { view: "agent", label: "Agent", Icon: Sparkles },
  { view: "paired", label: "Terminal", Icon: SquareTerminal },
  { view: "diff", label: "Diff", Icon: FileDiff },
  { view: "files", label: "Files", Icon: FolderTree },
];

/**
 * The phone's views of the open Session as a tab bar, as phone apps switch between a few views of one thing:
 * Agent, Terminal, Diff and Files when available, then More for the rest (Background, plugin panes, other
 * Sessions' shells). A view picked from More lights the More tab and names itself there.
 */
export function MobileViewTabs({
  view,
  available,
  terminal,
  moreLabel,
  onSelect,
  onMore,
}: {
  view: RightPanelView;
  /** Pane views this Session offers; Diff and Files tabs show only when listed. */
  available: readonly string[];
  /** Whether the Terminal tab shows. */
  terminal: boolean;
  /** The view More stands for while one of its items shows; otherwise it reads "More". */
  moreLabel: string | null;
  onSelect: (view: RightPanelView) => void;
  onMore: () => void;
}) {
  const tabs = TABS.filter((t) => t.view === "agent" || (t.view === "paired" ? terminal : available.includes(t.view)));
  const tab = (active: boolean) =>
    `flex-1 min-w-0 flex flex-col items-center justify-center gap-0.5 h-12 text-[11px] cursor-pointer transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500 ${
      active ? "text-text-primary" : "text-text-dim active:text-text-secondary"
    }`;
  return (
    <nav
      aria-label="Session views"
      data-testid="mobile-view-tabs"
      className="flex items-stretch shrink-0 px-1 border-b border-surface-700/40 bg-surface-900"
    >
      {tabs.map(({ view: v, label, Icon }) => {
        const active = view === v;
        return (
          <button
            key={v}
            type="button"
            aria-current={active ? "page" : undefined}
            data-testid={`mobile-tab-${v}`}
            onClick={() => onSelect(v)}
            className={tab(active)}
          >
            <Icon className="size-[18px]" strokeWidth={active ? 2 : 1.5} aria-hidden />
            <span className="max-w-full truncate px-1">{label}</span>
          </button>
        );
      })}
      <button
        type="button"
        aria-haspopup="dialog"
        aria-current={moreLabel ? "page" : undefined}
        data-testid="mobile-tab-more"
        onClick={onMore}
        className={tab(moreLabel !== null)}
      >
        <Ellipsis className="size-[18px]" strokeWidth={moreLabel ? 2 : 1.5} aria-hidden />
        <span className="max-w-full truncate px-1">{moreLabel ?? "More"}</span>
      </button>
    </nav>
  );
}
