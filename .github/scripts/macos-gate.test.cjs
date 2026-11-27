const { test } = require('node:test');
const assert = require('node:assert/strict');
const gate = require('./macos-gate.cjs');

const queueRef = `refs/heads/gh-readonly-queue/main/pr-42-${'a'.repeat(40)}`;

async function decide({ event = 'pr', files = [], labels = [], aheadBy = 1, prChecks = [] } = {}) {
  const notices = [];
  let output;
  const payload = event === 'pr'
    ? { pull_request: { number: 42 } }
    : { merge_group: { base_sha: 'b', base_ref: 'refs/heads/main', head_sha: 'h' } };
  const github = {
    rest: {
      pulls: {
        get: async () => ({ data: { labels: labels.map(name => ({ name })), head: { sha: 'p'.repeat(40) } } }),
        listFiles: 'listFiles',
      },
      repos: {
        compareCommitsWithBasehead: async ({ basehead }) => {
          if (basehead === 'refs/heads/main...h') return { data: { ahead_by: aheadBy } };
          assert.equal(basehead, 'b...h');
          return { data: { files, ahead_by: 1 } };
        },
      },
      checks: {
        listForRef: async ({ ref, check_name }) => {
          assert.equal(ref, 'p'.repeat(40));
          assert.equal(check_name, 'Cargo Test (macos)');
          return { data: { check_runs: prChecks.map(conclusion => ({ conclusion, app: { slug: 'github-actions' } })) } };
        },
      },
    },
    paginate: async (method, args) => {
      assert.equal(method, 'listFiles');
      assert.equal(args.pull_number, 42);
      return files;
    },
  };
  const core = { notice: m => notices.push(m), setOutput: (k, v) => { output = v; } };
  await gate({ github, context: { repo: {}, payload }, core, queueRef: event === 'pr' ? '' : queueRef });
  return { run: output, notice: notices.at(-1) };
}

test('path and cfg rules', async () => {
  const cases = [
    ['theme label edit in system.rs', [{ filename: 'src/server/api/system.rs', patch: '+label' }], 'false'],
    ['Linux-only process module', [{ filename: 'src/process/linux.rs', patch: '+x' }], 'false'],
    ['macOS process module', [{ filename: 'src/process/macos.rs', patch: '+x' }], 'true'],
    ['exact file', [{ filename: 'src/session/mod.rs', patch: '+x' }], 'true'],
    ['rename out of a sensitive path', [{ filename: 'src/a.rs', previous_filename: 'src/git/a.rs', patch: '+x' }], 'true'],
    ['added macOS cfg', [{ filename: 'src/a.rs', patch: '@@\n+#[cfg(target_os = "macos")]' }], 'true'],
    ['cfg only in context', [{ filename: 'src/a.rs', patch: '@@\n #[cfg(target_os = "macos")]\n+x' }], 'false'],
    ['Rust file without a patch', [{ filename: 'src/big.rs' }], 'true'],
    ['PR lockfile bump', [{ filename: 'Cargo.lock', patch: '+x' }], 'false'],
  ];
  for (const [name, files, run] of cases) {
    assert.equal((await decide({ files })).run, run, name);
  }
});

test('the label forces macOS', async () => {
  assert.equal((await decide({ labels: ['ci-macos'] })).notice, 'macOS runs: ci-macos label');
});

test('a PR over 300 files is inspected, not run by default', async () => {
  const files = Array.from({ length: 400 }, (_, i) => ({ filename: `web/${i}.ts`, patch: '+x' }));
  assert.equal((await decide({ files })).run, 'false');
  const huge = Array.from({ length: 3000 }, (_, i) => ({ filename: `web/${i}.ts`, patch: '+x' }));
  assert.equal((await decide({ files: huge })).notice, 'macOS runs: diff too large to inspect');
});

test('the queue checks the lockfile and fails closed on a large group', async () => {
  const lock = [{ filename: 'Cargo.lock', patch: '+x' }];
  assert.equal((await decide({ event: 'queue', files: lock })).run, 'true');
  const big = Array.from({ length: 300 }, (_, i) => ({ filename: `web/${i}.ts`, patch: '+x' }));
  assert.equal((await decide({ event: 'queue', files: big })).notice, 'macOS runs: diff too large to inspect');
  assert.equal((await decide({ event: 'queue', files: [{ filename: 'web/a.ts', patch: '+x' }] })).run, 'false');
});

test('the queue skips macOS only for a lone PR that passed it', async () => {
  const sensitive = [{ filename: 'src/git/a.rs', patch: '+x' }];
  const cases = [
    ['passed on the PR', { prChecks: ['success'] }, 'false'],
    ['passed, with the label', { prChecks: ['success'], labels: ['ci-macos'] }, 'false'],
    ['skipped on the PR', { prChecks: ['skipped'] }, 'true'],
    ['latest PR attempt failed', { prChecks: ['success', 'failure'] }, 'true'],
    ['no PR check', {}, 'true'],
    ['queued behind another entry', { prChecks: ['success'], aheadBy: 2 }, 'true'],
  ];
  for (const [name, opts, run] of cases) {
    assert.equal((await decide({ event: 'queue', files: sensitive, ...opts })).run, run, name);
  }
});
