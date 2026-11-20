/** Fired after a create pulled the latest Sandbox image, so the update banner asks again
 *  instead of waiting for its next poll. */
export const SANDBOX_IMAGE_PULLED_EVENT = "aoe:sandbox-image-pulled";
/** Fired when a create that starts or joins a pull is sent, so the banner polls fast and its
 *  chip shows that pull. */
export const SANDBOX_IMAGE_PULL_STARTED_EVENT = "aoe:sandbox-image-pull-started";

export function notifySandboxImagePulled(): void {
  window.dispatchEvent(new Event(SANDBOX_IMAGE_PULLED_EVENT));
}

export function onSandboxImagePulled(listener: () => void): () => void {
  window.addEventListener(SANDBOX_IMAGE_PULLED_EVENT, listener);
  return () => window.removeEventListener(SANDBOX_IMAGE_PULLED_EVENT, listener);
}

export function notifySandboxImagePullStarted(): void {
  window.dispatchEvent(new Event(SANDBOX_IMAGE_PULL_STARTED_EVENT));
}

export function onSandboxImagePullStarted(listener: () => void): () => void {
  window.addEventListener(SANDBOX_IMAGE_PULL_STARTED_EVENT, listener);
  return () => window.removeEventListener(SANDBOX_IMAGE_PULL_STARTED_EVENT, listener);
}
