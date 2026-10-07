import { useState } from "react";

import { lucideIcon, toneClasses, validTone } from "../../lib/pluginUi";
import { pluginLinkProps, renderIcon, safeHref, str, type Obj } from "./slotPayload";

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
  const chip = chipParts(item, slot, pluginId);
  if (!chip) return null;
  const safe = safeHref(str(item, "href"));
  const linkProps = safe ? pluginLinkProps(safe) : null;
  return linkProps ? (
    <a
      {...chip.common}
      {...linkProps}
      onClick={(e) => {
        e.stopPropagation();
        linkProps.onClick(e);
      }}
    >
      {chip.inner}
    </a>
  ) : (
    <span {...chip.common}>{chip.inner}</span>
  );
}

const itemText = (item: Obj) => str(item, "text")?.trim();
const renders = (item: Obj) => Boolean(lucideIcon(str(item, "icon")) || itemText(item));

function chipParts(item: Obj, slot: string, pluginId: string) {
  const text = itemText(item);
  const tooltip = str(item, "tooltip");
  const icon = lucideIcon(str(item, "icon"));
  if (!renders(item)) return null;
  // Only text badges truncate; an icon-only chip must not be squeezed and clip its icon.
  const fit = text ? "max-w-48 min-w-0 truncate" : "shrink-0";
  return {
    common: {
      className: `inline-flex items-center gap-1 font-mono text-[11px] px-1.5 py-0.5 rounded-full ${fit} ${toneClasses(validTone(item.tone))}`,
      title: tooltip || text || undefined,
      "aria-label": text ? undefined : tooltip || undefined,
      "data-plugin-slot": slot,
      "data-plugin-id": pluginId,
    },
    inner: (
      <>
        {renderIcon(icon, "size-3 shrink-0")}
        {text && <span className="truncate">{text}</span>}
      </>
    ),
  };
}

/** One chip showing `items[index]`; a click advances and wraps. The position is
 *  local to this tab and clamped when a re-push shrinks the group. */
function CyclingChip({
  items,
  group,
  slot,
  pluginId,
}: {
  items: Obj[];
  group: string;
  slot: string;
  pluginId: string;
}) {
  const [index, setIndex] = useState(0);
  const at = index % items.length;
  const item = items[at];
  const chip = item && chipParts(item, slot, pluginId);
  if (!chip) return null;
  const position = `${group} (${at + 1}/${items.length})`;
  return (
    <button
      type="button"
      {...chip.common}
      title={chip.common.title ? `${chip.common.title} (${at + 1}/${items.length})` : position}
      aria-label={chip.common["aria-label"] ?? (chip.common.title ? undefined : position)}
      className={`${chip.common.className} cursor-pointer`}
      onClick={(e) => {
        // A session row is a link: stop its handler and the browser following its href.
        e.stopPropagation();
        e.preventDefault();
        setIndex(at + 1);
      }}
    >
      {chip.inner}
    </button>
  );
}

type Segment = { key: string; group?: string; items: Obj[] };

/** Items sharing a non-empty `group` collapse into one cycling chip placed where
 *  the group first appears; ungrouped items stay separate chips. Items that
 *  render nothing are dropped first so they can neither hide the control nor
 *  count as a cycle step. */
function segmentItems(items: Obj[]): Segment[] {
  const segments: Segment[] = [];
  const groups = new Map<string, Segment>();
  items.forEach((item, i) => {
    if (!renders(item)) return;
    const group = str(item, "group");
    if (!group) {
      segments.push({ key: `i:${i}`, items: [item] });
      return;
    }
    const existing = groups.get(group);
    if (existing) existing.items.push(item);
    else {
      const segment = { key: `g:${group}`, group, items: [item] };
      groups.set(group, segment);
      segments.push(segment);
    }
  });
  return segments;
}

/** A badge's `items` list as chips, with grouped items cycling on click. */
export function BadgeItems({ items, slot, pluginId }: { items: Obj[]; slot: string; pluginId: string }) {
  return segmentItems(items).map(({ key, group, items: members }) => {
    const [first] = members;
    if (group && members.length > 1) {
      return <CyclingChip key={key} items={members} group={group} slot={slot} pluginId={pluginId} />;
    }
    return first && <BadgeChip key={key} item={first} slot={slot} pluginId={pluginId} />;
  });
}
