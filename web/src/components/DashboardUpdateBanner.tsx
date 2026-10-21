/** "Dashboard updated, reload" banner. */
export function DashboardUpdateBanner() {
  return (
    <div
      role="status"
      aria-label="Dashboard update available"
      className="bg-brand-600/10 border-b border-brand-600/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-brand-300 animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-brand-400 shrink-0" />
      <span>Dashboard updated.</span>
      <button
        type="button"
        onClick={() => window.location.reload()}
        className="underline hover:text-brand-200 cursor-pointer"
      >
        Reload now
      </button>
    </div>
  );
}
