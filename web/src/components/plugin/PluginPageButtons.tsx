import { useLocation } from "react-router-dom";
import { usePluginPages } from "../../lib/pluginUiContext";
import { pluginPagePath } from "../../lib/pluginPages";
import { requestNavigate } from "../../lib/sessionRoute";
import { PluginIdentityIcon } from "../settings/PluginIdentityIcon";

/** One TopBar button per plugin page, beside the palette pill; absent until a plugin declares one. */
export function PluginPageButtons() {
  const pages = usePluginPages();
  const { pathname } = useLocation();

  if (!pages?.length) return null;
  // Bounded and scrollable, so many pages never push the palette pill out of the header.
  return (
    <div className="flex shrink-0 max-w-[40%] items-center gap-1 overflow-x-auto">
      {pages.map((page) => {
        const path = pluginPagePath(page);
        const current = pathname === path;
        return (
          <button
            key={path}
            onClick={() => requestNavigate(path)}
            className={`w-8 h-8 shrink-0 flex items-center justify-center rounded-md cursor-pointer transition-colors ${
              current
                ? "text-text-primary bg-surface-700/60"
                : "text-text-muted hover:text-text-secondary hover:bg-surface-700/50"
            }`}
            aria-label={page.title}
            aria-current={current ? "page" : undefined}
            title={page.title}
          >
            <PluginIdentityIcon icon={page.icon} />
          </button>
        );
      })}
    </div>
  );
}
