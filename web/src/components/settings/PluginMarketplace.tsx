import { Sparkles } from "lucide-react";

import { BUTTON_PRIMARY_SM, BUTTON_SECONDARY, CARD, INPUT } from "../../lib/surfaces";
import { PanelMessage } from "../panelParts";
import { PILL } from "./PluginCard";
import { sourceFromCommand, type PluginMarketplaceState } from "./pluginFlows";
import { isImeComposing } from "../../lib/ime";

interface Props {
  market: PluginMarketplaceState;
  installedSources: Set<string | null>;
  onOpenDetail: (target: { source: string; title: string; installCommand: string }) => void;
}

/** The marketplace tab: GitHub search results, each installable in-app. */
export function PluginMarketplace({ market, installedSources, onOpenDetail }: Props) {
  const {
    query,
    setQuery,
    suggestionsOnly,
    setSuggestionsOnly,
    results,
    error,
    discovering,
    previewingSource,
    discover,
    install,
  } = market;
  const suggestingCount = results?.filter((r) => r.suggestions).length ?? 0;
  const shown = suggestionsOnly ? results?.filter((r) => r.suggestions) : results;
  // A filter chip, the picked one filled as the redesign marks a current choice.
  const chipClass = (active: boolean) =>
    `inline-flex h-8 max-md:h-10 items-center gap-1.5 rounded-lg border px-3 text-[12px] max-md:text-[14px] cursor-pointer transition-colors ${
      active
        ? "border-surface-600 bg-surface-700 text-text-primary"
        : "border-surface-700/60 text-text-secondary hover:bg-surface-800"
    }`;
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !isImeComposing(e)) void discover();
          }}
          placeholder="Search GitHub (aoe-plugin topic)…"
          className={`${INPUT} !w-auto min-w-0 flex-1`}
          data-testid="plugins-discover-query"
        />
        <button
          type="button"
          className={BUTTON_SECONDARY}
          disabled={discovering}
          onClick={() => void discover()}
          data-testid="plugins-discover"
        >
          {discovering ? "Searching…" : "Search GitHub"}
        </button>
      </div>

      {error && (
        <PanelMessage tone="error" className="px-1" testId="plugins-discover-error">
          {error}
        </PanelMessage>
      )}

      {results && results.length > 0 && (
        <div className="flex flex-wrap gap-2" data-testid="plugins-filter">
          <button
            type="button"
            className={chipClass(!suggestionsOnly)}
            aria-pressed={!suggestionsOnly}
            onClick={() => setSuggestionsOnly(false)}
            data-testid="plugins-filter-all"
          >
            All
          </button>
          <button
            type="button"
            className={chipClass(suggestionsOnly)}
            aria-pressed={suggestionsOnly}
            onClick={() => setSuggestionsOnly(true)}
            data-testid="plugins-filter-suggestions"
          >
            <Sparkles className="size-3.5" aria-hidden="true" />
            Works with Suggestions
            <span className="font-mono text-text-dim">{suggestingCount}</span>
          </button>
        </div>
      )}

      {shown && (
        <div className="space-y-2" data-testid="plugins-discover-results">
          {shown.length === 0 ? (
            <PanelMessage className="px-1">
              {results?.length === 0
                ? "No plugins found on the aoe-plugin topic."
                : "No plugins here work with Suggestions yet."}
            </PanelMessage>
          ) : (
            shown.map((r) => {
              const source = sourceFromCommand(r.install_command);
              return (
                <div
                  key={r.slug}
                  className={`${CARD} py-3 text-[13px]`}
                  data-testid={`plugins-discover-result-${r.slug}`}
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <img
                      src={r.source_avatar_url}
                      alt=""
                      aria-hidden="true"
                      className="size-4 shrink-0 rounded-full"
                      data-testid={`plugins-discover-avatar-${r.slug}`}
                      onError={(e) => {
                        e.currentTarget.classList.add("hidden");
                      }}
                    />
                    <button
                      type="button"
                      className="cursor-pointer font-medium text-text-primary hover:underline"
                      onClick={() => onOpenDetail({ source: r.slug, title: r.slug, installCommand: r.install_command })}
                      data-testid={`plugins-discover-open-${r.slug}`}
                    >
                      {r.slug}
                    </button>
                    <span className={`${PILL} bg-accent-500/20 text-accent-500`}>{r.badge}</span>
                    {r.suggestions && (
                      <span
                        className={`${PILL} inline-flex items-center gap-1 border border-brand-600/60 text-brand-400`}
                        title={`This plugin can suggest Sessions. Turn it on in Settings › Suggestions${r.badge === "installed" ? "" : " after installing"}.`}
                        data-testid={`plugins-suggestions-chip-${r.slug}`}
                      >
                        <Sparkles className="size-3" aria-hidden="true" />
                        Suggestions
                      </span>
                    )}
                    <span className="text-text-dim">★ {r.stars}</span>
                    <a href={r.html_url} target="_blank" rel="noreferrer" className="text-text-dim hover:underline">
                      GitHub ↗
                    </a>
                  </div>
                  {r.description && <p className="mt-1 leading-normal text-text-secondary">{r.description}</p>}
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    {r.badge === "installed" || installedSources.has(source) ? (
                      <span className="text-text-dim">Installed.</span>
                    ) : (
                      <button
                        type="button"
                        className={BUTTON_PRIMARY_SM}
                        disabled={previewingSource !== null}
                        onClick={() => void install(source)}
                        data-testid={`plugins-install-${r.slug}`}
                      >
                        {previewingSource === source ? "Checking…" : "Install"}
                      </button>
                    )}
                    <span className="text-[12px] text-text-dim">
                      or in a terminal: <code>{r.install_command}</code>
                    </span>
                  </div>
                </div>
              );
            })
          )}
        </div>
      )}
    </div>
  );
}
