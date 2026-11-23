import { lazy, Suspense, type ComponentType, type JSX, type ReactNode } from "react";

/** Code-split a view: renders `fallback` until `load` resolves, then the component with the same props. */
export function lazyView<P extends object>(load: () => Promise<ComponentType<P>>, fallback: ReactNode) {
  const Lazy = lazy(() => load().then((component) => ({ default: component })));
  return function LazyView(props: P) {
    return (
      <Suspense fallback={fallback}>
        <Lazy {...(props as P & JSX.IntrinsicAttributes)} />
      </Suspense>
    );
  };
}
