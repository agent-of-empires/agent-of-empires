import { useLocation } from "react-router-dom";
import { usePluginPages } from "../../lib/pluginUiContext";
import { pluginPagePath } from "../../lib/pluginPages";
import { requestNavigate } from "../../lib/sessionRoute";
import { PluginIdentityIcon } from "../settings/PluginIdentityIcon";

/** One TopBar button per plugin page, beside the palette pill; absent until a plugin declares one. */
export function PluginPageButtons() {
  const pages = usePluginPages();
  const { pathname } = useLocation();

  return pages?.map((page) => {
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
  });
}
