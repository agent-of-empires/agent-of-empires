// Decides whether `Cargo Test (macos)` runs for a pull_request or merge_group
// event. Fails closed: anything it cannot inspect runs macOS.

const PREFIXES = [
  'src/process/', 'src/git/', 'src/migrations/', 'src/containers/',
  'src/session/config/container_config/',
  '.github/actions/rust-toolchain/',
];
// Compiled only for Linux, so macOS never builds it.
const LINUX_ONLY = ['src/process/linux.rs'];
// Existing macOS-only bodies can change without touching their cfg.
const EXACT = [
  'src/hooks/dir_guard.rs',
  'src/server/api/sessions/file_edit.rs', 'src/session/anchored_fs.rs',
  'src/session/mod.rs', 'src/sound/playback.rs',
  'src/tui/clipboard.rs', 'src/tui/open_url.rs',
  'src/session/config/container_config.rs',
  'src/session/instance/container.rs',
  'src/session/sandbox_store_reclaim.rs',
  'build.rs', 'rust-toolchain.toml',
];
// A PR lockfile bump runs post-merge; the queue still checks it.
const QUEUE_ONLY = ['Cargo.lock'];
// pulls.listFiles stops listing here.
const FILE_LIMIT = 3000;
const CFG = /target_os\s*=\s*"macos"/;

function sensitivePath(path, queue) {
  if (!path || LINUX_ONLY.includes(path)) return false;
  return EXACT.includes(path) || (queue && QUEUE_ONLY.includes(path)) ||
    PREFIXES.some(p => path.startsWith(p));
}

// The first file that needs macOS, or undefined.
function sensitiveFile(files, queue) {
  return files.find(f =>
    sensitivePath(f.filename, queue) || sensitivePath(f.previous_filename, queue) ||
    // Only Rust and TOML hold the cfg; binaries have no patch.
    (!f.patch && /\.(rs|toml)$/.test(f.filename)) ||
    f.patch?.split('\n').some(line =>
      /^[+-](?![+-]{2} )/.test(line) && CFG.test(line)));
}

async function changedFiles(github, context, pr, queue) {
  if (pr) {
    return github.paginate(github.rest.pulls.listFiles, {
      ...context.repo, pull_number: pr.number, per_page: 100,
    });
  }
  // base_sha is the entry ahead of this one, so this lists only this PR.
  const { data } = await github.rest.repos.compareCommitsWithBasehead({
    ...context.repo, basehead: `${queue.base_sha}...${queue.head_sha}`,
  });
  // The compare API lists at most 300 files.
  return data.files.length >= 300 ? null : data.files;
}

module.exports = async function macosGate({ github, context, core, queueRef }) {
  const run = reason => { core.notice(`macOS runs: ${reason}`); core.setOutput('run', 'true'); };
  const pr = context.payload.pull_request;
  const queue = context.payload.merge_group;
  // refs/heads/gh-readonly-queue/<base>/pr-<number>-<base sha>
  const number = pr?.number ?? Number(/\/pr-(\d+)-[0-9a-f]+$/.exec(queueRef)?.[1]);
  if (!number) return run(`no PR number in ${queueRef}`);
  const { data: current } = await github.rest.pulls.get({ ...context.repo, pull_number: number });
  if (current.labels.some(l => l.name === 'ci-macos')) return run('ci-macos label');

  const files = await changedFiles(github, context, pr, queue);
  if (!files || files.length >= FILE_LIMIT) return run('diff too large to inspect');
  const hit = sensitiveFile(files, !pr);
  if (hit) return run(`${hit.filename} is macOS-sensitive`);
  core.notice('macOS skipped: runs post-merge on main. Add ci-macos and re-run this job to run it here.');
  core.setOutput('run', 'false');
};
