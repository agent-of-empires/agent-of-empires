import { useEffect, useState } from "react";
import { fetchSkills, type SkillsResponse } from "../lib/api";
import { buildSkillIndex, type SkillIndex } from "../lib/skillProvenance";

let skillsPromise: Promise<SkillIndex> | null = null;

const EMPTY_INDEX = buildSkillIndex(null);

function getSkillIndexOnce(): Promise<SkillIndex> {
  if (!skillsPromise) {
    skillsPromise = fetchSkills().then((res: SkillsResponse | null) => {
      // Don't cache a failure; badges are cosmetic.
      if (!res) {
        skillsPromise = null;
        return EMPTY_INDEX;
      }
      return buildSkillIndex(res);
    });
    skillsPromise = skillsPromise.catch(() => {
      skillsPromise = null;
      return EMPTY_INDEX;
    });
  }
  return skillsPromise;
}

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
