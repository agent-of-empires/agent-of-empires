import { useRef, useState } from "react";
import { LayoutGrid } from "lucide-react";
import { usePluginPages } from "../../lib/pluginUiContext";
import { pluginPagePath } from "../../lib/pluginPages";
import { requestNavigate } from "../../lib/sessionRoute";
import { PluginIdentityIcon } from "../settings/PluginIdentityIcon";
import { useOutsideDismiss } from "../useOutsideDismiss";

/** TopBar menu of plugin pages; absent until a plugin declares one. */
export function PluginPagesMenu() {
  const pages = usePluginPages();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useOutsideDismiss(open, [ref], () => setOpen(false));

  if (!pages?.length) return null;

  return (
    <div ref={ref} className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        className="w-8 h-8 flex items-center justify-center rounded-md cursor-pointer text-text-muted hover:text-text-secondary hover:bg-surface-700/50 transition-colors"
        aria-label="Pages"
        title="Pages"
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <LayoutGrid size={16} strokeWidth={1.5} aria-hidden="true" />
      </button>

      {open && (
        <div
          role="menu"
          aria-label="Pages"
          className="absolute right-0 top-full mt-1 min-w-[180px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          {pages.map((page) => (
            <button
              key={`${page.plugin_id}/${page.id}`}
              role="menuitem"
              onClick={() => {
                setOpen(false);
                requestNavigate(pluginPagePath(page));
              }}
              className="w-full flex items-center gap-2 text-left px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-700/60 hover:text-text-primary cursor-pointer"
            >
              <PluginIdentityIcon icon={page.icon} />
              {page.title}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
