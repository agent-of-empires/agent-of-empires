import type { AcpState } from "../../lib/acpTypes";

function isCursorSession(opts: {
  currentAgent: string | null;
  sessionTool?: string | null;
}): boolean {
  return (
    opts.sessionTool === "cursor" ||
    opts.currentAgent === "cursor" ||
    opts.currentAgent === "cursor-agent"
  );
}

export function showModePickerForComposerControls(opts: {
  currentAgent: string | null;
  sessionTool?: string | null;
}): boolean {
  return !isCursorSession(opts);
}

export function configOptionsForComposerControls(
  configOptions: AcpState["configOptions"],
  opts: {
    currentAgent: string | null;
    sessionTool?: string | null;
  },
): AcpState["configOptions"] {
  if (!isCursorSession(opts)) return configOptions;
  return configOptions.filter(
    (option) => option.category !== "model" && option.id !== "fast",
  );
}
