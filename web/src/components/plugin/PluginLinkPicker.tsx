import { useEffect } from "react";

import { openPluginLink, type CommandLink } from "../../lib/pluginCommands";
import { BACKDROP, MENU, MENU_HEADING } from "../../lib/surfaces";
import { MenuItem } from "../ContextMenu";

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
      className={`fixed inset-0 z-[90] flex items-center justify-center ${BACKDROP}`}
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Open link"
    >
      <div className={`min-w-[18rem] max-w-[92vw] ${MENU}`} onClick={(e) => e.stopPropagation()}>
        <div className={MENU_HEADING}>Open link</div>
        <ul className="flex flex-col">
          {links.map((link, i) => (
            <li key={link.href}>
              <MenuItem onClick={() => open(link.href)} trailing={i < 9 ? i + 1 : undefined}>
                {link.label}
              </MenuItem>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
