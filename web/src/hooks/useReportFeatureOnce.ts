import { useEffect } from "react";
import { reportFeatureUse, type FeatureUse } from "../lib/featureUse";

/** Counts one use each time `when` turns true, such as a section appearing. */
export function useReportFeatureOnce(feature: FeatureUse, when: boolean) {
  useEffect(() => {
    // Reports an appearance, not a handler: nothing else observes the section showing up.
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (when) reportFeatureUse(feature);
  }, [feature, when]);
}
