export const TOOLBAR_BUTTON = "w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors";
export const TOOLBAR_TINT = (on: boolean, onClass = "text-brand-500") =>
  on ? onClass : "text-text-dim hover:text-text-secondary";
export const DISABLED_ICON_BUTTON =
  "disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-text-muted disabled:hover:bg-transparent";
