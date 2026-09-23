import { createElement, type CSSProperties, type MouseEvent } from "react";

import type { LucideIcon } from "lucide-react";

import { isAllowedHref, isInternalHref, navigateInternalHref } from "../../lib/pluginHref";

export type Obj = Record<string, unknown>;

// Plugin strings are untrusted: only allowed hrefs (see isAllowedHref) ever become links.
export function safeHref(href: string | undefined): string | undefined {
  return href && isAllowedHref(href) ? href : undefined;
}

/** Anchor props for a `safeHref` result: an internal href navigates via the
 *  router on a plain click; anything else is a normal new-tab external link. */
export function pluginLinkProps(href: string) {
  const internal = isInternalHref(href);
  return {
    href,
    target: internal ? undefined : "_blank",
    rel: internal ? undefined : "noopener noreferrer",
    onClick: (e: MouseEvent<HTMLAnchorElement>) => {
      if (!internal || e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
        return;
      }
      e.preventDefault();
      navigateInternalHref(href);
    },
  };
}

export function isObject(v: unknown): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

export function str(obj: Obj, key: string): string | undefined {
  const v = obj[key];
  return typeof v === "string" ? v : undefined;
}

export function objectList(obj: Obj, key: string): Obj[] | undefined {
  const v = obj[key];
  return Array.isArray(v) ? v.filter(isObject) : undefined;
}

export function renderIcon(icon: LucideIcon | undefined, className: string, style?: CSSProperties) {
  return icon && createElement(icon, { className, style, "aria-hidden": true });
}
