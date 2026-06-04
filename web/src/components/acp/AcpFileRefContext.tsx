// Bridges a file-open handler from App down into the structured view markdown
// anchor override. The transcript text nodes are mounted by
// assistant-ui's MessagePrimitive.Parts, so we cannot prop-drill a
// callback to the Markdown component; context is the only injection
// point. StructuredView provides the handler; the anchor override in
// Markdown.tsx consumes it. See #1718.

import { createContext, useContext } from "react";
import type { FileRef } from "../../lib/fileRef";

export interface AcpFileRefContextValue {
  /** Open a local file reference cited in the transcript. Absent when
   *  the structured view is rendered without a file-open target, in which case
   *  the anchor override leaves links as normal (new-tab) anchors. */
  onOpenFileRef?: (ref: FileRef) => void;
}

export const AcpFileRefContext = createContext<AcpFileRefContextValue>(
  {},
);

export function useAcpFileRef(): AcpFileRefContextValue {
  return useContext(AcpFileRefContext);
}
