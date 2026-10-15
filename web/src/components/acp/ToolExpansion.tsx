/* eslint-disable react-refresh/only-export-components */
// Per-call expand state held above the cards, so it survives a card
// re-parenting (a run folding into a group) that remounts it.

import { createContext, useContext, useState, type ReactNode } from "react";

import type { ToolDensity } from "./ToolDisplayMode";

/** A user toggle, valid only for the density it was made in. */
export interface ExpansionOverride {
  density: ToolDensity;
  open: boolean;
}

export class ToolExpansionStore {
  private overrides = new Map<string, ExpansionOverride>();
  private listeners = new Set<() => void>();

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  get = (id: string) => this.overrides.get(id) ?? null;

  update(id: string, next: (prev: ExpansionOverride | null) => ExpansionOverride) {
    this.overrides.set(id, next(this.get(id)));
    for (const listener of this.listeners) listener();
  }

  /** Whether the call shows open under `density`: the reader's toggle, else `baseline`. */
  isOpen(id: string, density: ToolDensity, baseline: boolean) {
    const o = this.get(id);
    return o?.density === density ? o.open : baseline;
  }
}

const ToolExpansionContext = createContext<ToolExpansionStore | null>(null);
const ToolIdContext = createContext<string | null>(null);

export function ToolExpansionProvider({ children }: { children: ReactNode }) {
  const [store] = useState(() => new ToolExpansionStore());
  return <ToolExpansionContext.Provider value={store}>{children}</ToolExpansionContext.Provider>;
}

export function ToolIdProvider({ id, children }: { id: string; children: ReactNode }) {
  return <ToolIdContext.Provider value={id}>{children}</ToolIdContext.Provider>;
}

export const useToolExpansionStore = () => useContext(ToolExpansionContext);
export const useToolId = () => useContext(ToolIdContext);
