import { Spinner } from "./SandboxImageBanner";
import { useSandboxImage } from "../hooks/useSandboxImage";

/**
 * The running sandbox image pull while its banner is off screen: hidden with ×, or covered by a higher
 * banner. It brings the banner back only after ×; while a higher banner covers it, it just shows the pull.
 */
export function SandboxPullChip() {
  const sandbox = useSandboxImage();
  const running = sandbox?.banner.running;
  if (!sandbox || !running || (!running.hidden && !sandbox.higherBannerShown)) return null;
  const canShow = running.hidden && !sandbox.higherBannerShown;
  const { done, known } = running.progress;
  const counted = done !== undefined && known !== undefined;
  const what = counted ? `Downloading sandbox image, ${done} of ${known} layers` : "Downloading sandbox image";
  return (
    <button
      type="button"
      onClick={canShow ? running.onShow : undefined}
      aria-disabled={!canShow}
      aria-label={canShow ? `${what}. Show details` : what}
      data-testid="sandbox-pull-chip"
      className={`inline-flex items-center gap-1.5 h-8 px-2.5 rounded-md border border-surface-700 font-mono text-xs text-text-secondary ${
        canShow ? "cursor-pointer hover:text-text-primary hover:bg-surface-700/50" : "cursor-default"
      }`}
    >
      <Spinner size="h-3 w-3" />
      {counted ? `Image ${done}/${known}` : "Image"}
    </button>
  );
}
