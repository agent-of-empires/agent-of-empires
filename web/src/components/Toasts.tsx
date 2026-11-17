import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { toastBus, type ToastApi, type ToastKind } from "../lib/toastBus";
import { requestOpenSession } from "../lib/sessionRoute";
import { openPluginLink } from "../lib/pluginCommands";
import { isAllowedHref } from "../lib/pluginHref";

interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
  sessionId?: string;
  href?: string;
  action?: { label: string; run: () => boolean | void };
}

const ToastContext = createContext<ToastApi | null>(null);

const TOAST_LIFETIME_MS = 6000;
// The least time a toast stays once hover and focus have both ended.
const TOAST_GRACE_MS = 2000;

interface TimerState {
  handle: ReturnType<typeof setTimeout> | null;
  remaining: number;
  startedAt: number;
  hover: boolean;
  focus: boolean;
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextId = useRef(1);

  // Per-toast lifetime state. A timer is paused when its handle is null.
  const timers = useRef(new Map<number, TimerState>());

  const dismiss = useCallback((id: number) => {
    setToasts((t) => t.filter((toast) => toast.id !== id));
    const entry = timers.current.get(id);
    if (entry?.handle) clearTimeout(entry.handle);
    timers.current.delete(id);
  }, []);

  const startTimer = useCallback(
    (id: number, ms: number) => {
      const entry = timers.current.get(id);
      if (!entry) return;
      entry.startedAt = Date.now();
      entry.handle = setTimeout(() => dismiss(id), ms);
    },
    [dismiss],
  );

  const scheduleDismiss = useCallback(
    (id: number) => {
      timers.current.set(id, {
        handle: null,
        remaining: TOAST_LIFETIME_MS,
        startedAt: 0,
        hover: false,
        focus: false,
      });
      startTimer(id, TOAST_LIFETIME_MS);
    },
    [startTimer],
  );

  const pause = useCallback((id: number, source: "hover" | "focus") => {
    const entry = timers.current.get(id);
    if (!entry) return;
    entry[source] = true;
    if (entry.handle !== null) {
      clearTimeout(entry.handle);
      entry.remaining = Math.max(entry.remaining - (Date.now() - entry.startedAt), 0);
      entry.handle = null;
    }
  }, []);

  const resume = useCallback(
    (id: number, source: "hover" | "focus") => {
      const entry = timers.current.get(id);
      if (!entry) return;
      entry[source] = false;
      if (!entry.hover && !entry.focus && entry.handle === null) {
        startTimer(id, Math.max(entry.remaining, TOAST_GRACE_MS));
      }
    },
    [startTimer],
  );

  useEffect(() => {
    const map = timers.current;
    return () => {
      for (const t of map.values()) if (t.handle !== null) clearTimeout(t.handle);
      map.clear();
    };
  }, []);

  const push = useCallback(
    (message: string, kind: ToastKind = "info") => {
      const id = nextId.current++;
      setToasts((t) => [...t, { id, kind, message }]);
      scheduleDismiss(id);
    },
    [scheduleDismiss],
  );

  // Pushes a toast with an associated session so tapping it jumps to that session.
  const pushWithSession = useCallback(
    (message: string, sessionId: string) => {
      const id = nextId.current++;
      setToasts((t) => [...t, { id, kind: "info", message, sessionId }]);
      scheduleDismiss(id);
    },
    [scheduleDismiss],
  );

  // A click-to-open toast (worker `ui.open_url`): a browser blocks `window.open`
  // from an async push, so opening waits for the tap.
  const pushWithHref = useCallback(
    (message: string, href: string, kind: ToastKind = "info") => {
      const id = nextId.current++;
      setToasts((t) => [...t, { id, kind, message, href }]);
      scheduleDismiss(id);
    },
    [scheduleDismiss],
  );

  const pushWithAction = useCallback((message: string, label: string, run: () => boolean | void) => {
    const id = nextId.current++;
    // Stays until used or dismissed: its action can be the only way back to what failed.
    setToasts((t) => [...t, { id, kind: "error", message, action: { label, run } }]);
  }, []);

  const api = useMemo<ToastApi>(
    () => ({
      push,
      error: (m: string) => push(m, "error"),
      info: (m: string) => push(m, "info"),
      openLink: pushWithHref,
      action: pushWithAction,
    }),
    [push, pushWithHref, pushWithAction],
  );

  // Service worker forwards incoming push payloads here when the PWA is already visible and focused, so we show
  // the notification as an in-app toast instead of an OS lock-screen buzz.
  useEffect(() => {
    if (typeof navigator === "undefined" || !navigator.serviceWorker) return;
    const handler = (event: MessageEvent) => {
      const data = event.data as {
        type?: string;
        payload?: {
          title?: string;
          body?: string;
          session_id?: string;
        };
      } | null;
      if (!data || data.type !== "aoe-push" || !data.payload) return;
      const title = data.payload.title ?? "Agent of Empires";
      const body = data.payload.body ?? "";
      const message = body ? `${title}: ${body}` : title;
      const sessionId = data.payload.session_id;
      if (sessionId) {
        pushWithSession(message, sessionId);
      } else {
        push(message, "info");
      }
    };
    navigator.serviceWorker.addEventListener("message", handler);
    return () => {
      navigator.serviceWorker.removeEventListener("message", handler);
    };
  }, [push, pushWithSession]);

  // Action toasts stay until used, so the column can outgrow the screen; keep the newest in view.
  const listRef = useRef<HTMLDivElement>(null);
  const newestId = toasts.at(-1)?.id;
  // Ids only increase, so a smaller newest id means the newest was removed, not that one arrived.
  const scrolledFor = useRef(0);
  useLayoutEffect(() => {
    const list = listRef.current;
    if (!list || newestId === undefined || newestId <= scrolledFor.current) return;
    scrolledFor.current = newestId;
    list.scrollTop = list.scrollHeight;
  }, [newestId]);

  return (
    <ToastContext.Provider value={api}>
      {children}
      <div
        ref={listRef}
        data-testid="toast-list"
        className="fixed bottom-4 right-4 z-[80] flex flex-col gap-2 max-w-[92vw] sm:max-w-sm max-h-[calc(100dvh-2rem)] overflow-y-auto"
      >
        {toasts.map((t) => {
          const clickable = !!t.sessionId || !!t.href;
          // A blank message would leave the opener without an accessible name.
          const openerLabel = t.message.trim() ? t.message : t.href ? "Open link" : "Open session";
          const onToastClick = () => {
            if (t.href) {
              // Defensive re-check: the server already rejects disallowed notification hrefs, but never hand an
              // unvalidated scheme to window.open in case a future push path skips that gate.
              if (isAllowedHref(t.href)) openPluginLink(t.href);
            } else if (t.sessionId) {
              requestOpenSession(t.sessionId);
            } else {
              return;
            }
            dismiss(t.id);
          };
          return (
            <div
              key={t.id}
              role={t.kind === "error" ? "alert" : "status"}
              onClick={clickable ? onToastClick : undefined}
              onPointerEnter={() => pause(t.id, "hover")}
              onPointerLeave={() => resume(t.id, "hover")}
              onFocus={() => pause(t.id, "focus")}
              onBlur={(e) => {
                if (!e.currentTarget.contains(e.relatedTarget)) resume(t.id, "focus");
              }}
              className={`flex items-start gap-2 px-3 py-2 rounded-md border shadow-lg animate-slide-up text-sm ${
                t.kind === "error"
                  ? "bg-status-error/10 border-status-error/40 text-status-error"
                  : "bg-surface-800 border-surface-700 text-text-primary"
              } ${clickable ? "cursor-pointer hover:bg-surface-700 transition-colors" : ""}`}
            >
              {clickable ? (
                // The message is the toast's own control, so the keyboard can follow its link; a click on the
                // rest of the toast still opens it through the outer handler.
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    onToastClick();
                  }}
                  className="flex-1 break-words text-left cursor-pointer rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600"
                >
                  {openerLabel}
                </button>
              ) : (
                <span className="flex-1 break-words">{t.message}</span>
              )}
              {t.action && (
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    if (t.action!.run() !== false) dismiss(t.id);
                  }}
                  className="shrink-0 font-medium text-text-primary underline-offset-2 hover:underline cursor-pointer"
                >
                  {t.action.label}
                </button>
              )}
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  dismiss(t.id);
                }}
                className="text-text-dim hover:text-text-secondary cursor-pointer"
                aria-label="Dismiss"
              >
                &times;
              </button>
            </div>
          );
        })}
      </div>
    </ToastContext.Provider>
  );
}

/** Hook that wires the React ToastProvider into the module-level toastBus so non-React callers (like the fetch
 *  interceptor) can surface errors as toasts. */
export function ToastBusBridge() {
  const ctx = useContext(ToastContext);
  useEffect(() => {
    if (!ctx) return;
    toastBus.handler = ctx;
    return () => {
      if (toastBus.handler === ctx) toastBus.handler = null;
    };
  }, [ctx]);
  return null;
}
