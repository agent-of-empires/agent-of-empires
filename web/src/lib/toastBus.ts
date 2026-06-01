export type ToastKind = "error" | "info";

export interface ToastApi {
  push: (message: string, kind?: ToastKind) => void;
  error: (message: string) => void;
  info: (message: string) => void;
}

interface ToastBus {
  handler: ToastApi | null;
}

export const toastBus: ToastBus = { handler: null };

export function reportError(message: string): void {
  toastBus.handler?.error(message);
}

export function reportInfo(message: string): void {
  toastBus.handler?.info(message);
}
