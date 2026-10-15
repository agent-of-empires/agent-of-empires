import type { ButtonType, Options, Styles } from "react-joyride";

// Type-only joyride import keeps the engine out of the bundle. Colors use theme
// CSS variables, except overlayColor: an SVG fill where var() does not resolve.
export const TOUR_RUNNER_OPTIONS: Partial<Options> = {
  buttons: ["skip", "back", "primary"] as ButtonType[],
  showProgress: true,
  skipBeacon: true,
  // A scrim click can close without any callback in controlled mode, stranding the overlay.
  overlayClickAction: false,
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
