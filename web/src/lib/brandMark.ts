export const AOE_BRAND_MARK_COLORS = {
  backGradientStart: "#78350f",
  backGradientEnd: "#451a03",
  midGradientStart: "#92400e",
  midGradientEnd: "#78350f",
  frontGradientStart: "#f59e0b",
  frontGradientEnd: "#d97706",
  titlebarGradientStart: "#fbbf24",
  titlebarGradientEnd: "#f59e0b",
  detail: "#b45309",
  prompt: "#fef3c7",
  glow: "rgba(245,158,11,0.5), 0 0 48px rgba(245,158,11,0.25), 0 0 80px rgba(245,158,11,0.1)",
} as const;

export const AOE_BRAND_MARK_TEXT_SHADOW = `0 0 24px ${AOE_BRAND_MARK_COLORS.glow}`;
