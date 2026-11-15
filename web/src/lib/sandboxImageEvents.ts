/** Fired after a create pulled the latest Sandbox image, so the update banner asks again
 *  instead of waiting for its hourly poll. */
export const SANDBOX_IMAGE_PULLED_EVENT = "aoe:sandbox-image-pulled";

export function notifySandboxImagePulled(): void {
  window.dispatchEvent(new Event(SANDBOX_IMAGE_PULLED_EVENT));
}

export function onSandboxImagePulled(listener: () => void): () => void {
  window.addEventListener(SANDBOX_IMAGE_PULLED_EVENT, listener);
  return () => window.removeEventListener(SANDBOX_IMAGE_PULLED_EVENT, listener);
}
