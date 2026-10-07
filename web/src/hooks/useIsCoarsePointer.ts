import { useMediaQuery } from "./useMediaQuery";

export function useIsCoarsePointer(): boolean {
  return useMediaQuery("(pointer: coarse)");
}
