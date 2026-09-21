import { createElement, type CSSProperties } from "react";

import type { LucideIcon } from "lucide-react";

export type Obj = Record<string, unknown>;

// Plugin strings are untrusted: only http(s) hrefs ever become links.
export function safeHref(href: string | undefined): string | undefined {
  return href && /^https?:\/\//i.test(href) ? href : undefined;
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
