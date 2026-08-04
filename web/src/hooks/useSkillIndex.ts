import { useEffect, useState } from "react";
import { fetchSkills, type SkillsResponse } from "../lib/api";
import { buildSkillIndex, type SkillIndex } from "../lib/skillProvenance";

/** Module-level cache of the in-flight/completed `/api/skills` fetch, so
 *  every mounted slash-command popover and skill tool card shares one
 *  request per page load instead of each refetching independently. */
let skillsPromise: Promise<SkillIndex> | null = null;

const EMPTY_INDEX = buildSkillIndex(null);

function getSkillIndexOnce(): Promise<SkillIndex> {
  if (!skillsPromise) {
    skillsPromise = fetchSkills().then((res: SkillsResponse | null) => {
      // Do not cache a failure: badges are cosmetic, and one bad response at
      // page load should not disable them for the rest of the session.
      if (!res) {
        skillsPromise = null;
        return EMPTY_INDEX;
      }
      // Built once, not per mounted card; a long transcript can hold many.
      return buildSkillIndex(res);
    });
    // Every consumer awaits this one shared promise, so letting it reject would
    // turn a single bad response into an unhandled rejection per mounted card.
    skillsPromise = skillsPromise.catch(() => {
      skillsPromise = null;
      return EMPTY_INDEX;
    });
  }
  return skillsPromise;
}

/** Lookup from a slash-command/skill name to its provenance badge. Fetches
 *  `/api/skills` at most once per page load; returns the empty index until
 *  that fetch resolves. */
export function useSkillIndex(): SkillIndex {
  const [index, setIndex] = useState<SkillIndex>(EMPTY_INDEX);
  useEffect(() => {
    let cancelled = false;
    void getSkillIndexOnce().then((next) => {
      if (cancelled) return;
      setIndex(next);
    });
    return () => {
      cancelled = true;
    };
  }, []);
  return index;
}

/** Test-only seam: drop the cached skills fetch so each test starts cold. */
export function __resetSkillIndexCacheForTests(): void {
  skillsPromise = null;
}
