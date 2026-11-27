// Decides which expensive Tests jobs a pull_request run needs from the files
// it changes. Push and merge_group runs skip this job and run everything.

const LEGS = ['web-e2e', 'core-rest', 'bare-core'];
// pulls.listFiles stops listing here.
const FILE_LIMIT = 3000;

// No Rust code, test or build script reads these.
const isDocs = path => path.startsWith('docs/') || path.startsWith('website/') ||
  path.endsWith('.md');
// The dashboard bundle and its tests read only web/.
const isWeb = path => path.startsWith('web/') && !isDocs(path);
// Workflow and action edits change how every job runs.
const isCi = path => path.startsWith('.github/');

function classify(paths) {
  const web = paths.some(p => isWeb(p) || isCi(p));
  const code = paths.some(p => isCi(p) || !(isDocs(p) || isWeb(p)));
  return {
    web,
    // Live Playwright drives the real daemon, so Rust changes count too.
    live: web || code,
    // web-e2e builds the dashboard bundle; the other legs never read web/.
    legs: LEGS.filter(leg => code || (web && leg === 'web-e2e')),
  };
}

module.exports = async function changeGate({ github, context, core }) {
  const pr = context.payload.pull_request;
  const set = ({ web, live, legs }, reason) => {
    core.notice(`Tests: ${reason}; web jobs ${web ? 'run' : 'skip'}, live Playwright ${live ? 'runs' : 'skips'}, Cargo legs: ${legs.join(', ') || 'none'}.`);
    core.setOutput('web', String(web));
    core.setOutput('live', String(live));
    core.setOutput('legs', JSON.stringify(legs));
    core.setOutput('skipped_legs', JSON.stringify(LEGS.filter(leg => !legs.includes(leg))));
  };
  // The merge queue runs the real legs for a release PR.
  if (pr.head.repo?.full_name === pr.base.repo.full_name && pr.head.ref.startsWith('release-plz-')) {
    return set({ web: false, live: false, legs: [] }, 'release PR');
  }
  const files = await github.paginate(github.rest.pulls.listFiles, {
    ...context.repo, pull_number: pr.number, per_page: 100,
  });
  if (files.length >= FILE_LIMIT) {
    return set({ web: true, live: true, legs: LEGS }, 'diff too large to inspect');
  }
  const paths = files.flatMap(f => [f.filename, f.previous_filename].filter(Boolean));
  return set(classify(paths), `${files.length} changed files`);
};
