// Artifact images load through the auth-patched global fetch into a blob URL;
// a bare <img src> would carry no token.

import { useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { createPortal } from "react-dom";

import { useArtifactObjectUrl } from "../../hooks/useArtifactObjectUrl";
import { LIGHTBOX } from "../../lib/surfaces";

/** Thumbnail that opens full size in an in-page overlay; Esc or a click closes it.
 *  Focus moves to the close button while open (Tab stays put, nothing else is focusable),
 *  global key shortcuts are held back, and focus returns to the thumbnail on close.
 *  `wrapperClassName` positions the thumbnail button (e.g. `self-end` in a flex column). */
export function ExpandableImage({
  src,
  alt,
  className,
  wrapperClassName = "",
}: {
  src: string;
  alt?: string;
  className?: string;
  wrapperClassName?: string;
}) {
  const [open, setOpen] = useState(false);
  const thumbRef = useRef<HTMLButtonElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const thumb = thumbRef.current;
    closeRef.current?.focus();
    // Window capture runs before the app's document-capture shortcut dispatch, so no
    // global shortcut (e.g. terminal focus) reaches the page behind the modal.
    // Default actions stay, so Enter/Space still activate the close button.
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Escape") setOpen(false);
      else if (e.key === "Tab") e.preventDefault();
    };
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      thumb?.focus();
    };
  }, [open]);

  return (
    <>
      <button
        ref={thumbRef}
        type="button"
        title="Expand"
        className={`block max-w-full cursor-zoom-in ${wrapperClassName}`}
        onClick={() => setOpen(true)}
      >
        <img className={className} src={src} alt={alt ?? ""} />
      </button>
      {open &&
        createPortal(
          <div
            role="dialog"
            aria-modal="true"
            aria-label={alt || "Image"}
            data-testid="image-lightbox"
            className={`${LIGHTBOX} z-50 cursor-zoom-out`}
            onClick={() => setOpen(false)}
          >
            <button
              ref={closeRef}
              type="button"
              aria-label="Close image"
              className="absolute right-3 top-[max(12px,env(safe-area-inset-top))] inline-flex h-11 w-11 items-center justify-center cursor-pointer rounded-full bg-black/50 text-white hover:bg-black/70 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500"
              onClick={() => setOpen(false)}
            >
              <X className="h-5 w-5" />
            </button>
            <img src={src} alt={alt ?? ""} className="max-h-full max-w-full object-contain" />
          </div>,
          document.body,
        )}
    </>
  );
}

/** Alt text stands in while loading and on failure, never a broken image.
 *  `expandable` opens the loaded image in a lightbox. */
export function ArtifactImage({ url, alt, expandable }: { url: string; alt?: string; expandable?: boolean }) {
  const objectUrl = useArtifactObjectUrl(url);
  if (!objectUrl) {
    return <span className="acp-inert-path">{alt || "artifact"}</span>;
  }
  if (expandable) {
    return <ExpandableImage src={objectUrl} alt={alt} className="acp-artifact-image" wrapperClassName="self-end" />;
  }
  return <img className="acp-artifact-image" src={objectUrl} alt={alt ?? ""} />;
}
