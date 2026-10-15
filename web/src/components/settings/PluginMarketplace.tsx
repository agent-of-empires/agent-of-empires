import { sourceFromCommand, type PluginMarketplaceState } from "./pluginFlows";

interface Props {
  market: PluginMarketplaceState;
  installedSources: Set<string | null>;
  onOpenDetail: (target: { source: string; title: string; installCommand: string }) => void;
}

/** The marketplace tab: GitHub search results, each installable in-app. */
export function PluginMarketplace({ market, installedSources, onOpenDetail }: Props) {
  const { query, setQuery, results, error, discovering, previewingSource, discover, install } = market;
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void discover();
          }}
          placeholder="Search GitHub (aoe-plugin topic)…"
          className="min-w-0 flex-1 rounded border border-surface-700 bg-surface-850 px-2 py-1 text-xs"
          data-testid="plugins-discover-query"
        />
        <button
          type="button"
          className="rounded border border-surface-700 px-2 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
          disabled={discovering}
          onClick={() => void discover()}
          data-testid="plugins-discover"
        >
          {discovering ? "Searching…" : "Search GitHub"}
        </button>
      </div>

      {error && (
        <p className="text-xs text-status-error" data-testid="plugins-discover-error">
          {error}
        </p>
      )}

      {results && (
        <div className="space-y-2" data-testid="plugins-discover-results">
          {results.length === 0 ? (
            <p className="text-xs text-text-dim">No plugins found on the aoe-plugin topic.</p>
          ) : (
            results.map((r) => {
              const source = sourceFromCommand(r.install_command);
              return (
                <div
                  key={r.slug}
                  className="rounded border border-surface-700 bg-surface-850 p-2 text-xs"
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
                      className="font-medium text-accent-500 hover:underline"
                      onClick={() => onOpenDetail({ source: r.slug, title: r.slug, installCommand: r.install_command })}
                      data-testid={`plugins-discover-open-${r.slug}`}
                    >
                      {r.slug}
                    </button>
                    <span className="rounded bg-accent-500/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-accent-500">
                      {r.badge}
                    </span>
                    <span className="text-text-dim">★ {r.stars}</span>
                    <a href={r.html_url} target="_blank" rel="noreferrer" className="text-text-dim hover:underline">
                      GitHub ↗
                    </a>
                  </div>
                  {r.description && <p className="mt-1 text-text-dim">{r.description}</p>}
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    {r.badge === "installed" || installedSources.has(source) ? (
                      <span className="text-text-dim">Installed.</span>
                    ) : (
                      <button
                        type="button"
                        className="rounded bg-brand-600 px-2 py-0.5 text-[11px] font-medium text-white hover:bg-brand-500 disabled:opacity-50"
                        disabled={previewingSource !== null}
                        onClick={() => void install(source)}
                        data-testid={`plugins-install-${r.slug}`}
                      >
                        {previewingSource === source ? "Checking…" : "Install"}
                      </button>
                    )}
                    <span className="text-text-dim">
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
