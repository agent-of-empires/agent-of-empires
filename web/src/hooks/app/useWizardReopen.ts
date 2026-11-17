// Reopen the session wizard on a background create's failure, from its toast.

import { useCallback, useState } from "react";
import type { WizardRestore } from "../../components/session-wizard/SessionWizard";
import { toastBus } from "../../lib/toastBus";
import { useLatestRef } from "../useLatestRef";

export const REOPEN_REFUSED_MESSAGE = "Close the open session wizard to reopen this one.";

export function useWizardReopen(open: boolean, setOpen: (open: boolean) => void) {
  const [wizardRestore, setWizardRestore] = useState<WizardRestore | null>(null);
  // The toast keeps the handler from when it was raised, so the open state is read live.
  const openRef = useLatestRef(open);

  // Refuses while a wizard is open so its draft is not replaced; false keeps the toast.
  const reopenFailure = useCallback(
    (restore: WizardRestore): boolean => {
      if (openRef.current) {
        toastBus.handler?.info(REOPEN_REFUSED_MESSAGE);
        return false;
      }
      setWizardRestore(restore);
      setOpen(true);
      return true;
    },
    [openRef, setOpen],
  );

  const clearRestore = useCallback(() => setWizardRestore(null), []);

  return { wizardRestore, reopenFailure, clearRestore };
}
