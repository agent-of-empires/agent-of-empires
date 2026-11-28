import { createContext, useContext } from "react";
import type { SandboxImageBannerState } from "./useSandboxImageBanner";

export interface SandboxImageContextValue {
  banner: SandboxImageBannerState;
  /** A banner above the sandbox one in `BannerStack` is on screen, so the sandbox banner can't show. */
  higherBannerShown: boolean;
  setHigherBannerShown: (shown: boolean) => void;
}

export const SandboxImageContext = createContext<SandboxImageContextValue | null>(null);

/** Null outside `SandboxImageProvider`, so a component rendered on its own shows no sandbox state. */
export function useSandboxImage(): SandboxImageContextValue | null {
  return useContext(SandboxImageContext);
}
