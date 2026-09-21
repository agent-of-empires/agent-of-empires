// React error boundary that reports caught render errors to the client logger.
import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { reportError } from "../lib/logger";

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
}

interface State {
  hasError: boolean;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false };

  static getDerivedStateFromError(): State {
    return { hasError: true };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    reportError(error, {
      componentStack: info.componentStack ?? undefined,
      target: "react.errorboundary",
    });
  }

  render(): ReactNode {
    if (this.state.hasError) {
      return (
        this.props.fallback ?? (
          <div style={{ padding: 24 }}>
            <h2>Something went wrong</h2>
            <p>The error has been reported. Reload the page to retry.</p>
          </div>
        )
      );
    }
    return this.props.children;
  }
}
