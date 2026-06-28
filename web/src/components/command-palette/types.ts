import type { ReactNode } from "react";
import type { SessionStatus } from "../../lib/types";

export type CommandActionGroup = "Actions" | "Sessions" | "Conversations" | "Settings";

export interface CommandAction {
  id: string;
  title: string;
  subtitle?: string;
  group: CommandActionGroup;
  keywords?: string[];
  shortcut?: string;
  icon?: ReactNode;
  status?: SessionStatus;
  statusCreatedAt?: string | null;
  perform: () => void;
}
