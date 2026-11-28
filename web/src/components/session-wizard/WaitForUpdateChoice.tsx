import { useId } from "react";
import { layerCount, type SandboxPullProgress } from "../../hooks/useSandboxImageBanner";

/** Replaces "Pull latest image first" while the image is downloading: wait for that download, or start on
 *  the image already here. */
export function WaitForUpdateChoice({
  progress,
  wait,
  onChange,
}: {
  progress: SandboxPullProgress;
  wait: boolean;
  onChange: (wait: boolean) => void;
}) {
  const name = useId();
  const count = layerCount(progress);
  const options = [
    {
      value: true,
      title: "Wait for the update",
      hint: `An update is downloading${count ? ` (${count})` : ""}. This Session starts on the new image when it finishes.`,
    },
    {
      value: false,
      title: "Start now on the current image",
      hint: "To move it over later, use Recreate container in its menu.",
    },
  ];
  return (
    <div
      role="radiogroup"
      aria-label="Sandbox image"
      className="flex flex-col gap-1.5"
      data-testid="wizard-wait-for-update"
    >
      {options.map((option) => (
        <label key={String(option.value)} className="flex items-start gap-2 cursor-pointer">
          <input
            type="radio"
            name={name}
            className="mt-0.5 accent-brand-600"
            checked={wait === option.value}
            onChange={() => onChange(option.value)}
          />
          <span className="flex flex-col">
            <span className="text-sm text-text-secondary">{option.title}</span>
            <span className="text-xs text-text-dim">{option.hint}</span>
          </span>
        </label>
      ))}
    </div>
  );
}
