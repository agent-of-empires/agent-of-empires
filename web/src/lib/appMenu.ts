import type { OverflowItem } from "../components/OverflowMenu";

/** The app's own items, at the end of the header's ⋯ and in the phone session list's. */
export function appMenuItems(h: {
  onOpenHelp: () => void;
  onStartTutorial: () => void;
  onOpenTips: () => void;
  onOpenAbout: () => void;
  onLogout: () => void;
  loginRequired: boolean;
}): OverflowItem[] {
  const items: OverflowItem[] = [
    { label: "Help", onClick: h.onOpenHelp },
    { label: "Show tutorial", onClick: h.onStartTutorial },
    { label: "Tips", onClick: h.onOpenTips },
    { label: "About", onClick: h.onOpenAbout },
  ];
  if (h.loginRequired) items.push({ label: "Sign out", onClick: h.onLogout });
  return items;
}
