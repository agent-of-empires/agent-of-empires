// The redesign's shared surfaces, so every panel, menu and banner draws from one set rather than restyling its
// own. Elevation comes from the theme's `shadow-*` tokens (index.css); `float-control` and `glass-menu` are utilities
// there for the phone's floating controls and menus.

/** A phone's app banners (Daemon down, updates, install steps) as one card under the page's top controls. */
export const PHONE_BANNER_CARD =
  "rounded-xl overflow-hidden border border-surface-700/60 bg-surface-850 shadow-xl empty:hidden";
