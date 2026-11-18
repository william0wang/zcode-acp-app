import { Component, type ErrorInfo, type ReactNode } from "react";
import { AlertTriangle, Copy, RefreshCw } from "lucide-react";
import i18n from "../i18n";
import { useAppStore } from "../store/appStore";

interface State {
  error: Error | null;
}

// A render crash must never leave the dead black void (the reported
// "black screen on plan approval" with no recoverable info): catch it, show
// the message with the component stack, offer copy + reload. Class component
// by necessity — hooks-based boundaries don't exist.
export class ErrorBoundary extends Component<{ children: ReactNode }, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("render crash:", error, info.componentStack);
  }

  render() {
    const error = this.state.error;
    if (!error) return this.props.children;
    const details = error.stack ?? error.message;
    const copy = () => {
      void navigator.clipboard
        .writeText(details)
        .then(() => useAppStore.getState().notify(i18n.t("error.copied")))
        .catch(() => undefined);
    };
    return (
      <div className="flex h-full flex-col items-center justify-center gap-4 bg-canvas px-6 text-ink">
        <AlertTriangle className="size-8 text-warning" />
        <h1 className="text-lg font-semibold">{i18n.t("error.title")}</h1>
        <pre className="max-h-[50vh] w-full max-w-md overflow-auto whitespace-pre-wrap rounded-xl bg-raised px-4 py-3 font-mono text-xs text-dim ring-1 ring-hairline">
          {details}
        </pre>
        <div className="flex gap-2">
          <button
            onClick={copy}
            className="flex items-center gap-1.5 rounded-xl bg-raised px-4 py-2.5 text-sm text-dim ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
          >
            <Copy className="size-4" />
            {i18n.t("error.copy")}
          </button>
          <button
            onClick={() => window.location.reload()}
            className="flex items-center gap-1.5 rounded-xl bg-white px-4 py-2.5 text-sm font-semibold text-black active:scale-[0.99]"
          >
            <RefreshCw className="size-4" />
            {i18n.t("error.reload")}
          </button>
        </div>
      </div>
    );
  }
}
