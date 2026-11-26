import { BUTTON_SECONDARY, CARD, INPUT } from "../../lib/surfaces";
import { BUTTON_PRIMARY_SM } from "../../lib/surfaces";
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
  const { query, setQuery, results, error, discovering, previewingSource, discover, install } = market;
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

      {results && (
        <div className="space-y-2" data-testid="plugins-discover-results">
          {results.length === 0 ? (
            <PanelMessage className="px-1">No plugins found on the aoe-plugin topic.</PanelMessage>
          ) : (
            results.map((r) => {
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
