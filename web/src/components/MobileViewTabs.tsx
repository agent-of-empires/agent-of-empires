import type { RightPanelView } from "../lib/rightPanelView";

const TABS: readonly { view: RightPanelView; label: string }[] = [
  { view: "agent", label: "Agent" },
  { view: "paired", label: "Terminal" },
  { view: "diff", label: "Diff" },
  { view: "files", label: "Files" },
];

/**
 * The phone's views of the open Session as a segmented control, as phone apps switch between a few views of one thing:
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
  // A segmented pill, as iOS switches between a few views of one thing: the shown view sits raised.
  const tab = (active: boolean) =>
    `flex-1 min-w-0 h-8 px-1 flex items-center justify-center rounded-full text-[13px] font-medium cursor-pointer transition-[color,background-color,box-shadow] duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500 ${
      active
        ? "bg-surface-600/80 text-text-primary shadow-[0_1px_3px_rgb(0_0_0/0.4)]"
        : "text-text-dim active:text-text-secondary"
    }`;
  return (
    <nav aria-label="Session views" data-testid="mobile-view-tabs" className="shrink-0 px-3 pb-2 bg-surface-900">
      <div className="flex items-stretch gap-0.5 p-1 rounded-full float-control">
        {tabs.map(({ view: v, label }) => {
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
              <span className="truncate">{label}</span>
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
          <span className="truncate">{moreLabel ?? "More"}</span>
        </button>
      </div>
    </nav>
  );
}
