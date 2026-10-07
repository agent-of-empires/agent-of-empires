import { useMediaQuery } from "./useMediaQuery";

export function useIsWideViewport(): boolean {
  return useMediaQuery("(min-width: 768px)", true);
}
