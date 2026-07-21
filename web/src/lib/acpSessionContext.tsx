// Bridges the active session id down to the transcript's tool-call cards
// (#2986). assistant-ui mounts those card nodes itself, so props cannot be
// drilled to McpToolCard / SkillToolCard; a dedicated context is the injection
// point. StructuredView provides the id; the tool cards consume it to render
// per-session plugin `tool-card-badge` slots. Kept separate from
// AcpFileRefContext so plugin badges do not silently vanish when file-ref
// metadata happens to be absent.

import { createContext, useContext } from "react";

/** The id of the session whose transcript is mounted, or undefined when no
 *  session is active (in which case per-session slot renderers show nothing). */
export const AcpSessionContext = createContext<string | undefined>(undefined);

export function useAcpSessionId(): string | undefined {
  return useContext(AcpSessionContext);
}
