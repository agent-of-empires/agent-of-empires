/** Fired after a create pulled the latest Sandbox image, so the update banner asks again
 *  instead of waiting for its next poll. `image` is the one the create's dialog showed, when it knew it. */
export const SANDBOX_IMAGE_PULLED_EVENT = "aoe:sandbox-image-pulled";
/** Fired when a create that starts or joins a pull is sent, so the banner polls fast and its
 *  chip shows that pull. */
export const SANDBOX_IMAGE_PULL_STARTED_EVENT = "aoe:sandbox-image-pull-started";

export function notifySandboxImagePulled(image?: string): void {
  window.dispatchEvent(new CustomEvent<string | undefined>(SANDBOX_IMAGE_PULLED_EVENT, { detail: image }));
}

export function onSandboxImagePulled(listener: (image: string | undefined) => void): () => void {
  const handler = (event: Event) => listener((event as CustomEvent<string | undefined>).detail ?? undefined);
  window.addEventListener(SANDBOX_IMAGE_PULLED_EVENT, handler);
  return () => window.removeEventListener(SANDBOX_IMAGE_PULLED_EVENT, handler);
}

export function notifySandboxImagePullStarted(): void {
  window.dispatchEvent(new Event(SANDBOX_IMAGE_PULL_STARTED_EVENT));
}

export function onSandboxImagePullStarted(listener: () => void): () => void {
  window.addEventListener(SANDBOX_IMAGE_PULL_STARTED_EVENT, listener);
  return () => window.removeEventListener(SANDBOX_IMAGE_PULL_STARTED_EVENT, listener);
}
