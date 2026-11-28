// Decides which expensive Tests jobs a pull_request run needs from the files
// it changes. Push and merge_group runs skip this job and run everything.

const LEGS = ['web-e2e', 'core-rest', 'bare-core'];
// The compare API lists at most 300 files.
const FILE_LIMIT = 300;

// No Rust code, test or build script reads these.
const isDocs = path => path.startsWith('docs/') || path.startsWith('website/') ||
  path.endsWith('.md');
// Rust tests read web/src (mirrors, color scan) and web/tests/helpers (the
// fake ACP agent), so those count as Rust inputs as well as web ones.
const isRustReadWeb = path => path.startsWith('web/src/') || path.startsWith('web/tests/helpers/');
const isWeb = path => path.startsWith('web/') && !isDocs(path);
// The Vitest job also runs the acp-worker/aoe-agent adapter tests.
const isAdapter = path => path.startsWith('acp-worker/');
// Workflow and action edits change how every job runs.
const isCi = path => path.startsWith('.github/');

function classify(paths) {
  const web = paths.some(p => isWeb(p) || isAdapter(p) || isCi(p));
  const code = paths.some(p => isCi(p) || isRustReadWeb(p) || !(isDocs(p) || isWeb(p)));
  return {
    web,
    // Live Playwright drives the real daemon, so Rust changes count too.
    live: web || code,
    // web-e2e builds the dashboard bundle from any web/ file.
    legs: LEGS.filter(leg => code || (web && leg === 'web-e2e')),
  };
}

module.exports = async function changeGate({ github, context, core, release }) {
  const pr = context.payload.pull_request;
  const set = ({ web, live, legs }, reason) => {
    core.notice(`Tests: ${reason}; web jobs ${web ? 'run' : 'skip'}, live Playwright ${live ? 'runs' : 'skips'}, Cargo legs: ${legs.join(', ') || 'none'}.`);
    core.setOutput('web', String(web));
    core.setOutput('live', String(live));
    core.setOutput('legs', JSON.stringify(legs));
    core.setOutput('skipped_legs', JSON.stringify(LEGS.filter(leg => !legs.includes(leg))));
  };
  // The workflow's own release-PR rule, so the stubs and the jobs it skips agree.
  if (release) return set({ web: false, live: false, legs: [] }, 'release PR');
  // The event's own revisions, so a re-run classifies the commit it tests.
  const { data: diff } = await github.rest.repos.compareCommitsWithBasehead({
    ...context.repo, basehead: `${pr.base.sha}...${pr.head.sha}`,
  });
  if (diff.files.length >= FILE_LIMIT) {
    return set({ web: true, live: true, legs: LEGS }, 'diff too large to inspect');
  }
  const paths = diff.files.flatMap(f => [f.filename, f.previous_filename].filter(Boolean));
  return set(classify(paths), `${diff.files.length} changed files`);
};
