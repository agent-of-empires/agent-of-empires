import { useEffect, useMemo, useState } from "react";
import { fetchSettings, getSettingsSchema, updateSettings, type SettingsScope } from "../lib/api";
import { activeSettingsScope } from "../lib/appSettings";
import { reportError, reportInfo } from "../lib/toastBus";
import type { CommandAction } from "../components/command-palette/types";
import type { SettingsFieldDescriptor } from "../lib/types";

interface Args {
  open: boolean;
  readOnly: boolean;
  onOpenSettingsTab: (tab: string) => void;
}

function sectionToTab(section: string): string {
  if (section === "web") return "notifications";
  if (section === "acp") return "structured-view";
  return section;
}

export function useSettingsCommands({ open, readOnly, onOpenSettingsTab }: Args): CommandAction[] {
  const [schema, setSchema] = useState<SettingsFieldDescriptor[]>([]);
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [activeScope, setActiveScope] = useState<SettingsScope>("machine");
  const [reloadNonce, setReloadNonce] = useState(0);
  const request = useMemo(() => ({ open, reloadNonce }), [open, reloadNonce]);
  const [loadedRequest, setLoadedRequest] = useState<typeof request | null>(null);

  useEffect(() => {
    if (!request.open) return;
    let cancelled = false;
    void (async () => {
      const [s, scope] = await Promise.all([getSettingsSchema(), activeSettingsScope()]);
      if (cancelled) return;
      if (s) setSchema(s);
      const settings = await fetchSettings(scope);
      if (cancelled) return;
      if (settings) {
        setActiveScope(scope);
        setValues(settings as Record<string, unknown>);
        setLoadedRequest(request);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [request]);

  return useMemo(() => {
    const actions: CommandAction[] = [];
    for (const f of schema) {
      if (f.web_write.policy === "local_only") continue;

      const sectionValues = (values[f.section] ?? {}) as Record<string, unknown>;
      const current = sectionValues[f.field];
      const keywords = [f.section, f.field, f.category, f.label, f.widget.kind, "setting", "config"];
      const id = `setting:${f.section}.${f.field}`;

      // Telemetry must use its dedicated consent flow.
      const inlineToggle =
        f.section !== "telemetry" &&
        f.widget.kind === "toggle" &&
        f.web_write.policy === "allow" &&
        loadedRequest === request &&
        typeof current === "boolean" &&
        !readOnly;

      if (inlineToggle) {
        const isOn = current === true;
        const scope: SettingsScope = f.profile_overridable ? activeScope : "machine";
        const scopeLabel = scope === "machine" ? "Global" : scope.profile;
        actions.push({
          id,
          title: f.label,
          subtitle: `${isOn ? "On" : "Off"} · ${scopeLabel}`,
          group: "Settings",
          keywords,
          perform: () => {
            const next = !isOn;
            void (async () => {
              const patch = { [f.section]: { [f.field]: next } };
              const ok = await updateSettings(scope, patch);
              if (!ok) {
                reportError(`Failed to update ${f.label}`);
                return;
              }
              const where = scope === "machine" ? "" : ` (profile ${scope.profile})`;
              reportInfo(`${f.label} ${next ? "enabled" : "disabled"}${where}`);
              setReloadNonce((n) => n + 1);
            })();
          },
        });
        continue;
      }

      actions.push({
        id,
        title: f.label,
        subtitle: `Opens settings · ${f.category}`,
        group: "Settings",
        keywords,
        perform: () => onOpenSettingsTab(sectionToTab(f.section)),
      });
    }
    return actions;
  }, [schema, values, activeScope, readOnly, onOpenSettingsTab, loadedRequest, request]);
}
