// Shared "one context menu at a time" bus. Each open context menu subscribes
// to the "close" event and publishes on open via closeOtherContextMenus, so a
// second menu (e.g. long-pressing a different row on mobile, where document
// "click" listeners don't fire on touchstart) dismisses the first without
// lifting menu state up to a common parent. Used by the sidebar group headers
// and the Projects section rows. See #2212.
export const menuBus = new EventTarget();

export function closeOtherContextMenus(): void {
  menuBus.dispatchEvent(new Event("close"));
}
