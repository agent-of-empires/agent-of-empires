import { useEffect } from "react";

import { openPluginLink, type CommandLink } from "../../lib/pluginCommands";

/** Picker for an `open-ui-link` keybind that resolved to several links. Opens
 *  inside the key/click gesture so a remote dashboard is not popup-blocked. */
export function PluginLinkPicker({ links, onClose }: { links: CommandLink[]; onClose: () => void }) {
  const open = (href: string) => {
    openPluginLink(href);
    onClose();
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
        return;
      }
      // Modified digits stay browser shortcuts.
      if (!e.ctrlKey && !e.metaKey && !e.altKey && e.key >= "1" && e.key <= "9") {
        const idx = e.key.charCodeAt(0) - "1".charCodeAt(0);
        const link = links[idx];
        if (link) {
          e.preventDefault();
          open(link.href);
        }
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [links]);

  return (
    <div
      className="fixed inset-0 z-[90] flex items-center justify-center bg-black/40"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Open link"
    >
      <div
        className="min-w-[18rem] max-w-[92vw] rounded-md border border-surface-700 bg-surface-800 p-2 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-2 py-1 text-xs uppercase tracking-wide text-text-dim">Open link</div>
        <ul className="flex flex-col">
          {links.map((link, i) => (
            <li key={link.href}>
              <button
                onClick={() => open(link.href)}
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-sm text-text-primary hover:bg-surface-700"
              >
                <span className="w-4 text-text-dim">{i < 9 ? i + 1 : ""}</span>
                <span className="flex-1 truncate">{link.label}</span>
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
