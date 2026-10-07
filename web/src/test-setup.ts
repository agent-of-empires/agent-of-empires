import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// Unmount after each test so React work can't fire after jsdom teardown.
afterEach(() => {
  cleanup();
});

// Newer Node ships an unusable native localStorage that stops jsdom installing its own.
function storageWorks(name: "localStorage" | "sessionStorage"): boolean {
  try {
    const s = (globalThis as Record<string, unknown>)[name] as Storage | undefined;
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

// cmdk needs ResizeObserver and scrollIntoView, which jsdom lacks.
if (typeof globalThis.ResizeObserver === "undefined") {
  class ResizeObserverStub {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  }
  Object.defineProperty(globalThis, "ResizeObserver", {
    value: ResizeObserverStub,
    configurable: true,
    writable: true,
  });
}
if (typeof Element !== "undefined" && !Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function scrollIntoView(): void {};
}
