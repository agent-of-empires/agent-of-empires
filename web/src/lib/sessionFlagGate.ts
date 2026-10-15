import { createContext, useContext, type Context } from "react";

interface SessionFlagGate {
  /** Provider for the resolved value, read by the gate's hook. */
  Context: Context<boolean>;
  /** Read the flag from an `/api/settings` payload. */
  parse(settings: Record<string, unknown> | null | undefined): boolean;
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
export function sessionFlagGate(key: string, defaultValue: boolean): SessionFlagGate {
  const Ctx = createContext<boolean>(defaultValue);
  return {
    Context: Ctx,
    parse(settings) {
      const session = settings?.session;
      if (!session || typeof session !== "object") {
        return defaultValue;
      }
      const value = (session as Record<string, unknown>)[key];
      return value === !defaultValue ? !defaultValue : defaultValue;
    },
    use() {
      return useContext(Ctx);
    },
  };
}
