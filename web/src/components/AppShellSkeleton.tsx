// Presentational app-shell skeletons shown while the first startup fetches
// settle. They let a PWA cold launch paint the chrome immediately and fill in
// as data arrives, instead of holding a single blank surface that flips to a
// fully-populated UI all at once. Purely visual: no data, no fetches, so it is
// safe to render before auth resolves.

// A faint placeholder block. surface-800 reads as "content that hasn't landed"
// against the surface-900 body; the pulse is motion-safe so reduced-motion
// users get a static block (matches the sidebar's existing pulse usage).
const BLOCK = "rounded-md bg-surface-800 motion-safe:animate-pulse";

// Decreasing widths so the placeholder rows read as a ragged list rather than a
// solid bar. Tailwind fraction utilities keep it in-idiom (no arbitrary values).
const ROW_WIDTHS = ["w-11/12", "w-10/12", "w-9/12", "w-8/12", "w-7/12", "w-6/12"];

/** Faint stand-in for the sidebar's session list. Hidden below md, matching the
 *  real sidebar which is a closed drawer on mobile. */
function SidebarSkeleton() {
  return (
    <div className="hidden md:flex w-[280px] shrink-0 flex-col gap-3 border-r border-surface-800 bg-surface-900 p-3">
      <div className={`${BLOCK} h-7 w-2/3`} />
      <div className="flex flex-col gap-1.5">
        {ROW_WIDTHS.map((w) => (
          <div key={w} className={`${BLOCK} h-8 ${w}`} />
        ))}
      </div>
    </div>
  );
}

/** Main content placeholder: a heading block over a few ragged lines. Fills the
 *  main pane while the session list (or a session's view) loads. */
export function MainPaneSkeleton() {
  return (
    <div className="flex h-full flex-1 flex-col gap-3 p-4 animate-fade-in">
      <div className={`${BLOCK} h-7 w-40`} />
      <div className="flex flex-col gap-2">
        {ROW_WIDTHS.slice(0, 5).map((w) => (
          <div key={w} className={`${BLOCK} h-4 ${w}`} />
        ))}
      </div>
    </div>
  );
}

/** Full-frame skeleton: TopBar strip + sidebar (md+) + main-pane placeholder.
 *  Used at the top-level auth gate, before we know whether the app or the login
 *  screen will render, so it carries no real data. */
export function AppShellSkeleton() {
  return (
    <div className="h-dvh flex flex-col bg-surface-900 text-text-primary overflow-hidden safe-area-inset">
      <div className="h-12 shrink-0 flex items-center gap-2 bg-surface-850 px-3">
        <div className={`${BLOCK} h-6 w-6`} />
        <div className={`${BLOCK} h-4 w-32`} />
      </div>
      <div className="flex flex-1 min-h-0">
        <SidebarSkeleton />
        <div className="flex-1 flex flex-col min-h-0 min-w-0">
          <MainPaneSkeleton />
        </div>
      </div>
    </div>
  );
}
