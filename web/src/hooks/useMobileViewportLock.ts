import { useEffect } from "react";

// The dashboard has no document-level scrolling surface. Keep that invariant
// at the app boundary so a Safari gesture or focus adjustment cannot move a
// terminal, composer, or toolbar as a single page-sized sheet.
export function useMobileViewportLock() {
  useEffect(() => {
    const media = window.matchMedia?.("(pointer: coarse)");
    if (!media) return;

    const resetDocumentScroll = () => {
      if (!media.matches) return;
      if (window.scrollX !== 0 || window.scrollY !== 0 || document.documentElement.scrollTop !== 0) {
        window.scrollTo(0, 0);
      }
    };

    const onScroll = () => {
      resetDocumentScroll();
      requestAnimationFrame(resetDocumentScroll);
    };

    resetDocumentScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    media.addEventListener?.("change", resetDocumentScroll);
    return () => {
      window.removeEventListener("scroll", onScroll);
      media.removeEventListener?.("change", resetDocumentScroll);
    };
  }, []);
}
