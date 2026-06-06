export function scrollLatestAssistantMessageIntoView(
  viewport: HTMLElement | null,
): boolean {
  if (!viewport) return false;
  const messages = viewport.querySelectorAll<HTMLElement>(
    '[data-acp-message-role="assistant"]',
  );
  const target = messages.item(messages.length - 1);
  if (!target) return false;
  target.scrollIntoView({ block: "start", behavior: "smooth" });
  return true;
}
