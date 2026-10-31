import { useEffect, useState, type ReactNode } from "react";
import { fetchSuggestions } from "../lib/api";
import { loadFeaturedSuggestionSources, sourceLabel, type FeaturedSources } from "../lib/featuredSuggestionSources";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { isWebUiStateHydrated, onWebUiStateHydrated } from "../lib/webUiHydration";
import { SuggestionsSetupSheet } from "./SuggestionsSetupSheet";

/** Synced to the server (see webUiSync), so one dismissal covers every device. */
const DISMISSED_KEY = "aoe-suggestions-hint-dismissed";

type Featured = { status: "loading" } | FeaturedSources;

interface Props {
  /** Open Settings › Plugins › Marketplace with the Suggestions filter on. */
  onOpenMarketplace: () => void;
  /** The sheet finished; the suggestions section should reload. */
  onFinished?: () => void;
  /** The suggestions view shows the sentence even after the homepage hint was dismissed. */
  ignoreDismissed?: boolean;
  /** The caller already knows no provider is installed, so the hint skips its own snapshot fetch. */
  noProvider?: boolean;
  /** Rendered instead of the hint when there is nothing to offer. */
  fallback?: ReactNode;
}

/** First-run hint while a featured provider exists and none is installed. */
export function SuggestionsSetupHint({
  onOpenMarketplace,
  onFinished,
  ignoreDismissed = false,
  noProvider: noProviderProp,
  fallback = null,
}: Props) {
  const [dismissed, setDismissed] = useState(false);
  const [noProvider, setNoProvider] = useState(false);
  const [featured, setFeatured] = useState<Featured>({ status: "loading" });
  const [sheetOpen, setSheetOpen] = useState(false);
  const hidden = dismissed && !ignoreDismissed;

  // Waits for web UI state hydration so a dismissal made on another Device is seen before any lookup.
  // Once on mount: later dismissals only hide what was already looked up.
  useEffect(() => {
    let alive = true;
    const run = async () => {
      if (safeGetItem(DISMISSED_KEY) === "1" && !ignoreDismissed) {
        setDismissed(true);
        return;
      }
      if (noProviderProp === false) return;
      if (noProviderProp === undefined) {
        const snapshot = await fetchSuggestions();
        if (!alive || !snapshot || snapshot.providers.length > 0 || !snapshot.prefs.homepage) return;
      }
      setNoProvider(true);
      const next = await loadFeaturedSuggestionSources();
      if (alive) setFeatured(next);
    };
    let off = () => {};
    if (isWebUiStateHydrated()) {
      void run();
    } else {
      off = onWebUiStateHydrated(() => {
        off();
        void run();
      });
    }
    return () => {
      alive = false;
      off();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const retry = async () => {
    setFeatured({ status: "loading" });
    setFeatured(await loadFeaturedSuggestionSources());
  };

  const dismiss = () => {
    safeSetItem(DISMISSED_KEY, "1");
    setDismissed(true);
  };

  if (!noProvider) return null;
  if (hidden) return fallback;
  if (featured.status === "loading") return null;
  if (featured.status === "error") {
    return (
      <p
        className="mt-3.5 flex items-center justify-center gap-2 text-xs text-text-dim"
        role="status"
        data-testid="suggestions-hint-error"
      >
        Couldn't look up suggestion sources. {featured.message}
        <button
          type="button"
          className="min-h-8 cursor-pointer font-medium text-brand-500 hover:underline"
          onClick={() => void retry()}
        >
          Try again
        </button>
        <DismissButton onClick={dismiss} />
      </p>
    );
  }
  if (featured.sources.length === 0) return fallback;
  const names = joinNames(featured.sources.map((r) => sourceLabel(r.slug)));

  return (
    <>
      <p
        className="mt-3.5 flex flex-wrap items-center justify-center gap-x-2 text-center text-xs text-text-dim"
        data-testid="suggestions-hint"
      >
        <span>Want AoE to suggest what to work on next, from your {names} issues and PRs?</span>
        <button
          type="button"
          className="min-h-8 cursor-pointer font-medium text-brand-500 hover:underline"
          onClick={() => setSheetOpen(true)}
          data-testid="suggestions-hint-open"
        >
          Set up suggestions
        </button>
        {!ignoreDismissed && <DismissButton onClick={dismiss} />}
      </p>
      {sheetOpen && (
        <SuggestionsSetupSheet
          sources={featured.sources}
          onClose={() => setSheetOpen(false)}
          onOpenMarketplace={() => {
            setSheetOpen(false);
            onOpenMarketplace();
          }}
          onFinished={() => {
            setSheetOpen(false);
            setNoProvider(false);
            onFinished?.();
          }}
        />
      )}
    </>
  );
}

function DismissButton({ onClick }: { onClick: () => void }) {
  return (
    <button
      type="button"
      title="Don't show again"
      aria-label="Don't show again"
      className="size-8 cursor-pointer rounded-md text-text-dim hover:bg-surface-800"
      onClick={onClick}
      data-testid="suggestions-hint-dismiss"
    >
      ✕
    </button>
  );
}

const joinNames = (names: string[]) =>
  names.length < 2 ? (names[0] ?? "") : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
