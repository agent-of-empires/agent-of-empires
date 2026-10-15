export const MODAL_INPUT =
  "w-full bg-surface-900 border border-surface-700 rounded px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none focus:border-brand-600 disabled:opacity-50";
export const MODAL_CANCEL =
  "px-3 py-1 text-sm text-text-secondary hover:bg-surface-700/50 rounded cursor-pointer transition-colors";
export const MODAL_PRIMARY =
  "px-3 py-1 text-sm text-text-primary bg-brand-600 hover:bg-brand-500 rounded cursor-pointer transition-colors";
export const TOOLBAR_BUTTON = "w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors";
export const TOOLBAR_TINT = (on: boolean, onClass = "text-brand-500") =>
  on ? onClass : "text-text-dim hover:text-text-secondary";
export const DISABLED_ICON_BUTTON =
  "disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-text-muted disabled:hover:bg-transparent";
