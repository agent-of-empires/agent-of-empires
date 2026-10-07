// File-ref handler and repo roots for transcript renderers, which assistant-ui mounts
// out of prop-drilling reach.

import { createContext, useContext } from "react";
import type { FileRef, FileRefSession } from "../../lib/fileRef";

export interface AcpFileRefContextValue {
  onOpenFileRef?: (ref: FileRef) => void;
  fileRefSession?: FileRefSession | null;
}

export const AcpFileRefContext = createContext<AcpFileRefContextValue>({});

export function useAcpFileRef(): AcpFileRefContextValue {
  return useContext(AcpFileRefContext);
}
