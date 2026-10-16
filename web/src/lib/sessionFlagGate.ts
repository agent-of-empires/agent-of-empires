import { createContext, useContext, type Context } from "react";

import type { SettingsResponse } from "./api";
import type { SettingsField } from "./generated/settingsKeys";

interface SessionFlagGate {
  /** Provider for the resolved value, read by the gate's hook. */
  Context: Context<boolean>;
  /** Read the flag from an `/api/settings` payload. */
  parse(settings: SettingsResponse | null | undefined): boolean;
  /** Read the value the nearest provider resolved. */
  use(): boolean;
}

/** A boolean `session.*` setting the dashboard reads from `/api/settings` and
 *  hands down by context.
 *
 *  `defaultValue` is what an older daemon that omits the field gets, and what
 *  the gate reports before the first settings fetch resolves. Only the
 *  opposite of the default flips it, so a missing or malformed value is never
 *  read as a deliberate choice. */
export function sessionFlagGate(key: SettingsField<"session">, defaultValue: boolean): SessionFlagGate {
  const Ctx = createContext<boolean>(defaultValue);
  return {
    Context: Ctx,
    parse(settings) {
      const value = settings?.session?.[key];
      return value === !defaultValue ? !defaultValue : defaultValue;
    },
    use() {
      return useContext(Ctx);
    },
  };
}
