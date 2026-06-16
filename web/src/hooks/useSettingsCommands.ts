import { useEffect, useMemo, useState } from "react";
import { fetchProfiles, fetchSettings, getSettingsSchema, updateProfileSettings } from "../lib/api";
import { reportError, reportInfo } from "../lib/toastBus";
import type { CommandAction } from "../components/command-palette/types";
import type { SettingsFieldDescriptor } from "../lib/types";

interface Args {
  /** Palette open state. Schema + values are (re)fetched on the rising edge so
   *  toggle subtitles reflect the latest saved state without a global cache. */
  open: boolean;
  /** Read-only servers 403 every write, so every entry becomes a jump. */
  readOnly: boolean;
  /** Jump to a settings tab (e.g. `/settings/sandbox`). */
  onOpenSettingsTab: (tab: string) => void;
}

/** Most schema sections render under a same-named settings tab; these two are
 *  the only exceptions in SettingsView's tab layout. */
function sectionToTab(section: string): string {
  if (section === "web") return "notifications";
  if (section === "acp") return "structured-view";
  return section;
}

/**
 * Per-setting command-palette entries (#2108) built from the settings schema
 * (single source of truth, #1692).
 *
 * Writable `toggle` fields flip inline (pessimistic PATCH + toast); every other
 * widget, plus elevation-gated toggles and everything in read-only mode, jumps
 * to its settings tab. `local_only` fields are omitted because the server PATCH
 * rejects them.
 *
 * Inline writes target the default profile, the same path SettingsView takes
 * when its profile picker sits on the default, so there is no new write
 * behavior. The write scope is named in the subtitle and toast rather than left
 * implicit: a `profile_overridable` flip names the profile, a global-only flip
 * reads "Global".
 */
export function useSettingsCommands({ open, readOnly, onOpenSettingsTab }: Args): CommandAction[] {
  const [schema, setSchema] = useState<SettingsFieldDescriptor[]>([]);
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [defaultProfile, setDefaultProfile] = useState("default");
  // Bumped after an inline flip to re-run the load effect, so a toggle's
  // subtitle reflects the value it was just set to.
  const [reloadNonce, setReloadNonce] = useState(0);

  // The palette stays mounted, so it must (re)fetch each time it becomes
  // visible: this keeps toggle subtitles fresh and, on a login-required
  // server, picks up data that was 401-empty before sign-in.
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
