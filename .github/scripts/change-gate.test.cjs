const { test } = require('node:test');
const assert = require('node:assert/strict');
const gate = require('./change-gate.cjs');

async function decide({ files = [], release = false } = {}) {
  const outputs = {};
  const payload = { pull_request: { base: { sha: 'b' }, head: { sha: 'h' } } };
  const github = {
    rest: {
      repos: {
        compareCommitsWithBasehead: async ({ basehead }) => {
          assert.equal(basehead, 'b...h');
          return { data: { files: files.map(f => (typeof f === 'string' ? { filename: f } : f)) } };
        },
      },
    },
  };
  const core = { notice: () => {}, setOutput: (k, v) => { outputs[k] = v; } };
  await gate({ github, context: { repo: {}, payload }, core, release });
  return outputs;
}

const ALL = '["web-e2e","core-rest","bare-core"]';

test('changed paths pick the jobs', async () => {
  const cases = [
    ['docs only', ['docs/a.md', 'website/x.mjs', 'README.md'], 'false', 'false', '[]'],
    ['web outside what Rust reads', ['web/tests/a.spec.ts', 'web/package.json', 'web/AGENTS.md'], 'true', 'true', '["web-e2e"]'],
    ['web/src, which Rust tests scan', ['web/src/a.ts'], 'true', 'true', ALL],
    ['fake ACP agent', ['web/tests/helpers/fakeAcpAgent.mjs'], 'true', 'true', ALL],
    ['adapter', ['acp-worker/aoe-agent/package.json'], 'true', 'true', ALL],
    ['Rust only', ['src/a.rs'], 'false', 'true', ALL],
    ['workflow edit', ['.github/workflows/tests.yml'], 'true', 'true', ALL],
    ['rename out of web', [{ filename: 'src/a.rs', previous_filename: 'web/tests/a.ts' }], 'true', 'true', ALL],
    ['rename into docs', [{ filename: 'docs/a.md', previous_filename: 'src/a.rs' }], 'false', 'true', ALL],
  ];
  for (const [name, files, web, live, legs] of cases) {
    const out = await decide({ files });
    assert.deepEqual([out.web, out.live, out.legs], [web, live, legs], name);
    const skipped = JSON.parse(out.skipped_legs);
    assert.deepEqual([...JSON.parse(legs), ...skipped].sort(), JSON.parse(ALL).sort(), `${name}: legs partition`);
  }
});

test('a release PR reports every leg as skipped without listing files', async () => {
  assert.equal((await decide({ files: ['src/a.rs'], release: true })).skipped_legs, ALL);
});

test('a diff too large to list runs everything', async () => {
  const files = Array.from({ length: 300 }, (_, i) => `docs/${i}.md`);
  const out = await decide({ files });
  assert.deepEqual([out.web, out.live, out.legs], ['true', 'true', ALL]);
});
