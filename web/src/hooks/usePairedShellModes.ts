import { useCallback, useState } from "react";

import type { ShellMode } from "../components/PairedTerminal";

/** Host or Container per Session and terminal tab, so choosing one can never switch another Session's shell. */
export function usePairedShellModes() {
  const [modes, setModes] = useState<ReadonlyMap<string, ShellMode>>(new Map());
  const modeFor = useCallback(
    (sessionId: string | null, terminalIndex: number): ShellMode =>
      (sessionId && modes.get(`${sessionId}:${terminalIndex}`)) || "host",
    [modes],
  );
  const setMode = useCallback((sessionId: string, terminalIndex: number, mode: ShellMode) => {
    setModes((current) => new Map(current).set(`${sessionId}:${terminalIndex}`, mode));
  }, []);
  return { modeFor, setMode };
}
