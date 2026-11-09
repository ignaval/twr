import { Component, type ErrorInfo, type ReactNode } from "react";
import { errorMessage } from "./errors";

/** The app's last resort: a render error shows what happened and a way back, instead of
 * a blank window. Used by both the desktop and the mobile entry points. */
export class ErrorBoundary extends Component<
  { children: ReactNode },
  { error?: unknown }
> {
  state: { error?: unknown } = {};
  static getDerivedStateFromError(error: unknown) {
    return { error };
  }
  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error(error, info.componentStack);
    try {
      window.twrApp?.reportError({
        message: errorMessage(error).slice(0, 4000),
        stack: (
          (error instanceof Error ? error.stack || "" : "") +
          (info.componentStack || "")
        ).slice(0, 16000),
        source: "render",
      });
    } catch {}
  }
  render() {
    if (this.state.error === undefined) return this.props.children;
    return (
      <main className="app-crash" role="alert" aria-labelledby="twr-crash">
        <h1 id="twr-crash">Something went wrong</h1>
        <p>
          The window stopped drawing because of an error. Your hub and its
          agents keep running. Reload to continue.
        </p>
        <pre>{errorMessage(this.state.error) || "Unknown error"}</pre>
        <button
          type="button"
          className="primary"
          autoFocus
          onClick={() => window.location.reload()}
        >
          Reload
        </button>
      </main>
    );
  }
}
