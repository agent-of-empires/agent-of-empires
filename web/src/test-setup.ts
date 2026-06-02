import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// React 19's scheduler can leave work pending past the end of a test file.
// When jsdom is torn down between files, any remaining work fires in a
// setImmediate callback and crashes with "ReferenceError: window is not defined"
// (originating in node_modules/react-dom/cjs/react-dom-client.development.js).
// Unmounting every rendered tree after each test prevents that pending work.
afterEach(() => {
  cleanup();
});

// Node >= 22 ships a native `localStorage`/`sessionStorage` gated behind
// `--localstorage-file`. On newer majors (observed on Node 26) that native
// global occupies the slot and is unusable without the flag, and jsdom then
// installs no Storage of its own, so every test that touches storage fails with
// "Cannot read properties of undefined". CI runs Node 22, where jsdom's Storage
// works and this whole block is skipped via the feature probe below.
//
// When storage is broken, install a self-contained in-memory implementation.
// A real class is used (not a bare object) and assigned to `globalThis.Storage`
// so both access patterns the suite relies on keep working: direct
// `localStorage.setItem(...)` and `vi.spyOn(Storage.prototype, "setItem")` (used
// to simulate quota/security errors). A matching lenient `StorageEvent` is
// installed too, since jsdom's brand-checks its `storageArea` against jsdom's
// own Storage, which no longer exists once we replace it.
function storageWorks(name: "localStorage" | "sessionStorage"): boolean {
  try {
    const s = (globalThis as Record<string, unknown>)[name] as
      | Storage
      | undefined;
    if (!s || typeof s.setItem !== "function") return false;
    s.setItem("__aoe_probe__", "1");
    s.removeItem("__aoe_probe__");
    return true;
  } catch {
    return false;
  }
}

function installInMemoryStorage(): void {
  class MemoryStorage {
    private store = new Map<string, string>();
    get length(): number {
      return this.store.size;
    }
    clear(): void {
      this.store.clear();
    }
    getItem(key: string): string | null {
      return this.store.has(key) ? (this.store.get(key) as string) : null;
    }
    key(index: number): string | null {
      return Array.from(this.store.keys())[index] ?? null;
    }
    removeItem(key: string): void {
      this.store.delete(key);
    }
    setItem(key: string, value: string): void {
      this.store.set(String(key), String(value));
    }
  }

  // Lenient StorageEvent: jsdom's validates `storageArea` is a jsdom Storage,
  // which we have replaced. Extends the (working) global Event so dispatch and
  // `instanceof` behave normally.
  interface StorageEventInitLike extends EventInit {
    key?: string | null;
    oldValue?: string | null;
    newValue?: string | null;
    url?: string;
    storageArea?: Storage | null;
  }
  class MemoryStorageEvent extends Event {
    key: string | null;
    oldValue: string | null;
    newValue: string | null;
    url: string;
    storageArea: Storage | null;
    constructor(type: string, init: StorageEventInitLike = {}) {
      super(type, init);
      this.key = init.key ?? null;
      this.oldValue = init.oldValue ?? null;
      this.newValue = init.newValue ?? null;
      this.url = init.url ?? "";
      this.storageArea = init.storageArea ?? null;
    }
  }

  const define = (name: string, value: unknown): void => {
    Object.defineProperty(globalThis, name, {
      value,
      configurable: true,
      writable: true,
    });
  };

  define("Storage", MemoryStorage);
  define("localStorage", new MemoryStorage());
  define("sessionStorage", new MemoryStorage());
  define("StorageEvent", MemoryStorageEvent);
}

if (!storageWorks("localStorage") || !storageWorks("sessionStorage")) {
  installInMemoryStorage();
}
