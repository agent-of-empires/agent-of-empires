import { useEffect, useMemo, useState } from "react";
import { fetchProfiles, fetchSettings, getSettingsSchema, updateProfileSettings } from "../lib/api";
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
  const [defaultProfile, setDefaultProfile] = useState("default");
  const [reloadNonce, setReloadNonce] = useState(0);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    void (async () => {
      const [s, profiles] = await Promise.all([getSettingsSchema(), fetchProfiles()]);
      if (cancelled) return;
      if (s) setSchema(s);
      const profile = profiles.find((p) => p.is_default)?.name ?? "default";
      setDefaultProfile(profile);
      const settings = await fetchSettings(profile);
      if (cancelled) return;
      if (settings) setValues(settings as Record<string, unknown>);
    })();
    return () => {
      cancelled = true;
    };
  }, [open, reloadNonce]);

  return useMemo(() => {
    const actions: CommandAction[] = [];
    for (const f of schema) {
      if (f.web_write.policy === "local_only") continue;

      const sectionValues = (values[f.section] ?? {}) as Record<string, unknown>;
      const current = sectionValues[f.field];
      const keywords = [f.section, f.field, f.category, f.label, f.widget.kind, "setting", "config"];
      const id = `setting:${f.section}.${f.field}`;

      const inlineToggle = f.widget.kind === "toggle" && f.web_write.policy === "allow" && !readOnly;

      if (inlineToggle) {
        const isOn = current === true;
        const scope = f.profile_overridable ? defaultProfile : "Global";
        actions.push({
          id,
          title: f.label,
          subtitle: `${isOn ? "On" : "Off"} · ${scope}`,
          group: "Settings",
          keywords,
          perform: () => {
            const next = !isOn;
            void (async () => {
              const ok = await updateProfileSettings(defaultProfile, { [f.section]: { [f.field]: next } });
              if (!ok) {
                reportError(`Failed to update ${f.label}`);
                return;
              }
              const where = f.profile_overridable ? ` (profile ${defaultProfile})` : "";
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
  }, [schema, values, defaultProfile, readOnly, onOpenSettingsTab]);
}
