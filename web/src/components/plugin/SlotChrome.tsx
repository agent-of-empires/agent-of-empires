import { lucideIcon, toneClasses, validTone } from "../../lib/pluginUi";
import { renderIcon, safeHref, str, type Obj } from "./slotPayload";

export function Spinner({ className }: { className: string }) {
  return (
    <svg className={`animate-spin ${className}`} viewBox="0 0 24 24" aria-hidden>
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
    </svg>
  );
}

/** One pill from `{ text, icon, tone, href, tooltip }`, a link when `href` is safe. */
export function BadgeChip({ item, slot, pluginId }: { item: Obj; slot: string; pluginId: string }) {
  const text = str(item, "text");
  const tooltip = str(item, "tooltip");
  const icon = lucideIcon(str(item, "icon"));
  if (!icon && !text) return null;
  const safe = safeHref(str(item, "href"));
  // Only text badges truncate; an icon-only chip must not be squeezed and clip its icon.
  const fit = text ? "max-w-48 min-w-0 truncate" : "shrink-0";
  const common = {
    className: `inline-flex items-center gap-1 font-mono text-[11px] px-1.5 py-0.5 rounded-full ${fit} ${toneClasses(validTone(item.tone))}`,
    title: tooltip || text || undefined,
    "aria-label": text ? undefined : tooltip || undefined,
    "data-plugin-slot": slot,
    "data-plugin-id": pluginId,
  };
  const inner = (
    <>
      {renderIcon(icon, "size-3 shrink-0")}
      {text && <span className="truncate">{text}</span>}
    </>
  );
  return safe ? (
    <a {...common} href={safe} target="_blank" rel="noopener noreferrer" onClick={(e) => e.stopPropagation()}>
      {inner}
    </a>
  ) : (
    <span {...common}>{inner}</span>
  );
}
