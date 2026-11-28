import { useState, type ReactNode } from "react";
import { SandboxImageContext } from "../hooks/useSandboxImage";
import { useSandboxImageBanner } from "../hooks/useSandboxImageBanner";

/** One sandbox image poll, shared by the banner and the top bar's pull chip. */
export function SandboxImageProvider({ children }: { children: ReactNode }) {
  const banner = useSandboxImageBanner();
  const [higherBannerShown, setHigherBannerShown] = useState(false);
  return (
    <SandboxImageContext.Provider value={{ banner, higherBannerShown, setHigherBannerShown }}>
      {children}
    </SandboxImageContext.Provider>
  );
}
