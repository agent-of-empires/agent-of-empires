import type { ReactNode, SVGProps } from "react";

/** The stroke-icon `<svg>` shell: callers set the size and stroke width and supply the paths. */
export function StrokeIcon({
  size,
  strokeWidth,
  className,
  hidden = false,
  children,
}: {
  size: number;
  strokeWidth: string;
  className?: string;
  hidden?: boolean;
  children: ReactNode;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden={hidden ? "true" : undefined}
    >
      {children}
    </svg>
  );
}

export function FoldChevron(props: SVGProps<SVGSVGElement>) {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" fill="currentColor" {...props}>
      <path
        d="M2 3 L5 6.5 L8 3"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function PlusIcon({
  size,
  strokeWidth = "2",
  round = true,
}: {
  size: number;
  strokeWidth?: string;
  round?: boolean;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin={round ? "round" : undefined}
    >
      <line x1="12" y1="5" x2="12" y2="19" />
      <line x1="5" y1="12" x2="19" y2="12" />
    </svg>
  );
}
