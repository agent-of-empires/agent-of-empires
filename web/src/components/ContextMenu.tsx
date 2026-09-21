import type { ReactNode, RefObject } from "react";
import { createPortal } from "react-dom";

export function ContextMenu({
  menu,
  menuRef,
  testId,
  minWidth = "min-w-[190px]",
  children,
}: {
  menu: { x: number; y: number };
  menuRef: RefObject<HTMLDivElement | null>;
  testId: string;
  minWidth?: string;
  children: ReactNode;
}) {
  return createPortal(
    <div
      ref={menuRef}
      data-testid={testId}
      className={`fixed z-50 bg-surface-800 border border-surface-700 rounded-lg shadow-lg py-1 ${minWidth} overflow-y-auto`}
      style={{ left: menu.x, top: menu.y, maxHeight: "calc(100dvh - 16px)" }}
    >
      {children}
    </div>,
    document.body,
  );
}

export function MenuItem({
  onClick,
  testId,
  icon,
  indent = false,
  flex = icon != null,
  className = "text-text-secondary hover:bg-surface-700/50",
  children,
}: {
  onClick: () => void;
  testId?: string;
  icon?: ReactNode;
  indent?: boolean;
  flex?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      data-testid={testId}
      className={`w-full text-left ${indent ? "pl-6 pr-3" : "px-3"} py-2 md:py-2 max-md:py-3 text-sm ${className} cursor-pointer transition-colors${flex ? " flex items-center gap-2" : ""}`}
    >
      {icon}
      {children}
    </button>
  );
}

export function MenuSeparator() {
  return <div className="border-t border-surface-700/20 my-1" />;
}

export function MenuHeading({ children }: { children: ReactNode }) {
  return <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">{children}</div>;
}
