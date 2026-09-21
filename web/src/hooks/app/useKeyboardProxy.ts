// Hidden textarea that holds the mobile soft keyboard open across session and view switches.

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  clearMobileKeyboardProxyInput,
  deliverMobileKeyboardProxyInput,
  forwardTerminalBeforeInput,
} from "../../lib/mobileKeyboardProxy";
import type { RightPanelView } from "../../lib/rightPanelView";

const isPhoneTouch = () => window.innerWidth < 768 && navigator.maxTouchPoints > 0;

export function useKeyboardProxy(activeSessionId: string | null, visibleView: RightPanelView) {
  const proxyRef = useRef<HTMLTextAreaElement>(null);
  const [proxy, setProxy] = useState<HTMLTextAreaElement | null>(null);
  const setProxyElement = useCallback((element: HTMLTextAreaElement | null) => {
    proxyRef.current = element;
    setProxy(element);
  }, []);

  const focus = useCallback(() => {
    if (isPhoneTouch()) proxyRef.current?.focus();
  }, []);

  const close = useCallback(() => {
    if (!isPhoneTouch()) return;
    proxyRef.current?.blur();
    if (document.activeElement instanceof HTMLTextAreaElement) document.activeElement.blur();
  }, []);

  // Input typed for one session or view must never be delivered to the next.
  const sessionIdRef = useRef(activeSessionId);
  const viewRef = useRef(visibleView);
  const transition = useCallback((nextSessionId: string | null, nextView: RightPanelView) => {
    if (sessionIdRef.current === nextSessionId && viewRef.current === nextView) return;
    sessionIdRef.current = nextSessionId;
    viewRef.current = nextView;
    if (proxyRef.current) proxyRef.current.value = "";
    clearMobileKeyboardProxyInput();
  }, []);

  useLayoutEffect(() => {
    transition(activeSessionId, visibleView);
  }, [activeSessionId, visibleView, transition]);

  useEffect(() => {
    if (!proxy) return;
    const onBeforeInput = (e: InputEvent) => forwardTerminalBeforeInput(e, deliverMobileKeyboardProxyInput);
    proxy.addEventListener("beforeinput", onBeforeInput);
    return () => proxy.removeEventListener("beforeinput", onBeforeInput);
  }, [proxy]);

  return { setProxyElement, focus, close, transition };
}
