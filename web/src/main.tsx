// Import order matters: logging, then token capture, then legacy URL redirect.
import "./logging-init";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./lib/token";
import "./lib/legacySessionRedirect";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { ToastBusBridge, ToastProvider } from "./components/Toasts";
import { installFetchErrorToasts } from "./lib/fetchInterceptor";
import { installEditingFocus } from "./lib/editingFocus";
import "./index.css";

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js");
}

installFetchErrorToasts();
installEditingFocus();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary>
      <ToastProvider>
        <ToastBusBridge />
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </ToastProvider>
    </ErrorBoundary>
  </StrictMode>,
);
