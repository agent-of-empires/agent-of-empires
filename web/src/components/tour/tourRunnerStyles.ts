import type { ButtonType, Options, Styles } from "react-joyride";

// react-joyride theming for the tour, kept in a non-component module so the
// component file can stay fast-refresh clean while these stay unit-testable.
// Only `import type` from react-joyride here, so this module does not pull the
// engine into the bundle (TourRunner stays the sole runtime importer).
//
// Theme via the app's resolved-theme CSS variables (web/src/index.css) so the
// tooltip tracks light vs dark instead of being pinned to dark hex. These land
// as inline CSS styles, where var() resolves. The exception is overlayColor: it
// is painted as an SVG fill attribute, where var() does not reliably resolve,
// so the scrim stays a theme-agnostic translucent black.
export const TOUR_RUNNER_OPTIONS: Partial<Options> = {
  buttons: ["skip", "back", "primary"] as ButtonType[],
  showProgress: true,
  skipBeacon: true,
  primaryColor: "var(--color-brand-600)",
  overlayColor: "rgba(0, 0, 0, 0.65)",
  textColor: "var(--color-text-primary)",
  zIndex: 10_000,
  scrollOffset: 96,
};

export const TOUR_RUNNER_STYLES: Partial<Styles> = {
  tooltip: {
    backgroundColor: "var(--color-surface-800)",
    border: "1px solid var(--color-surface-700)",
    borderRadius: 10,
    color: "var(--color-text-primary)",
    fontSize: 13,
  },
  tooltipTitle: {
    color: "var(--color-brand-500)",
    fontSize: 14,
    fontWeight: 600,
  },
  tooltipContent: { padding: "10px 4px" },
  buttonPrimary: {
    backgroundColor: "var(--color-brand-600)",
    borderRadius: 6,
    color: "var(--color-text-on-brand)",
  },
  buttonBack: { color: "var(--color-text-secondary)" },
  buttonSkip: { color: "var(--color-text-dim)" },
};
