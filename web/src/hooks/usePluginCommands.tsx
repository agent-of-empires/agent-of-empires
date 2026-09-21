import { type ReactElement, useEffect, useMemo, useState } from "react";

import { fetchPluginCommands, type PluginCommand, type PluginUiEntry } from "../lib/api";
import type { CommandAction } from "../components/command-palette/types";
import {
  buildPluginCommandActions,
  invokeActionlessCommand,
  openExternal,
  pickKeybindEffect,
  type CommandLink,
} from "../lib/pluginCommands";
import { PluginLinkPicker } from "../components/plugin/PluginLinkPicker";
import { listen } from "./domEvents";
import { useLatestRef } from "./useLatestRef";

export function usePluginCommands(
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): { actions: CommandAction[]; overlay: ReactElement | null } {
  const [commands, setCommands] = useState<PluginCommand[]>([]);
  const [pickerLinks, setPickerLinks] = useState<CommandLink[] | null>(null);

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

  const live = useLatestRef({ commands, entries, activeSessionId });
  useEffect(() => {
    const handler = (e: Event) => {
      if (e.defaultPrevented) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      const { commands, entries, activeSessionId } = live.current;
      const effect = pickKeybindEffect(commands, entries, activeSessionId, e as KeyboardEvent);
      if (!effect) return;
      e.preventDefault();
      if (effect.kind === "open") {
        openExternal(effect.href);
      } else if (effect.kind === "pick") {
        setPickerLinks(effect.links);
      } else if (activeSessionId) {
        invokeActionlessCommand(effect.cmd, activeSessionId);
      }
    };
    return listen(handler, [document, "keydown"]);
  }, [live]);

  const overlay = pickerLinks ? <PluginLinkPicker links={pickerLinks} onClose={() => setPickerLinks(null)} /> : null;

  return { actions, overlay };
}
