import { useEffect, useMemo, useRef, useState } from "react";

import { fetchPluginCommands, type PluginCommand, type PluginUiEntry } from "../lib/api";
import type { CommandAction } from "../components/command-palette/types";
import { buildPluginCommandActions, openExternal, pickKeybindHref } from "../lib/pluginCommands";

/** Surfaces active plugin commands as palette actions and binds their declared
 *  keybinds. `open-ui-link` commands open the active session's PR href from the
 *  UI-state snapshot, synchronously in the key/click handler so a remote
 *  dashboard is not popup-blocked. The keybind fires whenever the href resolves,
 *  independent of whether the (href-gated) palette entry is shown. */
export function usePluginCommands(entries: PluginUiEntry[], activeSessionId: string | null): CommandAction[] {
  const [commands, setCommands] = useState<PluginCommand[]>([]);

  useEffect(() => {
    let alive = true;
    void fetchPluginCommands().then((res) => {
      if (alive && res) setCommands(res.commands);
    });
    return () => {
      alive = false;
    };
  }, []);

  const actions = useMemo(
    () => buildPluginCommandActions(commands, entries, activeSessionId),
    [commands, entries, activeSessionId],
  );

  // The listener reads live state through a ref so it registers once rather than
  // re-binding on every ui-state poll.
  const live = useRef({ commands, entries, activeSessionId });
  useEffect(() => {
    live.current = { commands, entries, activeSessionId };
  });
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      const { commands, entries, activeSessionId } = live.current;
      const href = pickKeybindHref(commands, entries, activeSessionId, e);
      if (href) {
        e.preventDefault();
        openExternal(href);
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, []);

  return actions;
}
