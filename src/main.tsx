import { Component, type ErrorInfo, type ReactNode } from "react";
import ReactDOM from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import App from "./App";
import "./design-tokens.css";
import "./styles.css";

class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("界面异常", error, info);
    void invoke("log_frontend_error", {
      message: `${error.stack ?? error.message}\n${info.componentStack ?? ""}`,
    }).catch(() => {});
  }

  render() {
    if (this.state.error) {
      return (
        <div className="crash-screen">
          <h2>界面出现异常</h2>
          <p>{this.state.error.message}</p>
          <button type="button" className="btn primary" onClick={() => location.reload()}>
            重新加载
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>,
);
