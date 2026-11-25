import { lazy, Suspense, type ComponentType, type JSX, type ReactNode } from "react";
import { ErrorBoundary } from "../components/ErrorBoundary";

const loadFailed = (
  <div className="flex h-full flex-col items-center justify-center gap-3 bg-surface-900 text-text-dim">
    <div className="text-xs font-mono uppercase tracking-wide">Couldn't load this view.</div>
    <button
      type="button"
      onClick={() => window.location.reload()}
      className="text-xs font-mono uppercase tracking-wide underline cursor-pointer"
    >
      Reload
    </button>
  </div>
);

/** Code-split a view: renders `fallback` (given the view's props if a function) until `load` resolves, then the component with the same props. */
export function lazyView<P extends object>(
  load: () => Promise<ComponentType<P>>,
  fallback: ReactNode | ((props: P) => ReactNode),
) {
  const Lazy = lazy(() => load().then((component) => ({ default: component })));
  return function LazyView(props: P) {
    return (
      <ErrorBoundary fallback={loadFailed}>
        <Suspense fallback={typeof fallback === "function" ? fallback(props) : fallback}>
          <Lazy {...(props as P & JSX.IntrinsicAttributes)} />
        </Suspense>
      </ErrorBoundary>
    );
  };
}
