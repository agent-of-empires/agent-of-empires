const { test } = require('node:test');
const assert = require('node:assert/strict');
const gate = require('./change-gate.cjs');

async function decide({ files = [], headRef = 'feature', fork = false } = {}) {
  const outputs = {};
  const repo = { full_name: 'o/r' };
  const payload = { pull_request: {
    number: 42, base: { repo }, head: { ref: headRef, repo: fork ? { full_name: 'x/r' } : repo },
  } };
  const github = {
    rest: { pulls: { listFiles: 'listFiles' } },
    paginate: async (method, args) => {
      assert.equal(method, 'listFiles');
      assert.equal(args.pull_number, 42);
      return files.map(f => (typeof f === 'string' ? { filename: f } : f));
    },
  };
  const core = { notice: () => {}, setOutput: (k, v) => { outputs[k] = v; } };
  await gate({ github, context: { repo: {}, payload }, core });
  return outputs;
}

const ALL = '["web-e2e","core-rest","bare-core"]';

test('changed paths pick the jobs', async () => {
  const cases = [
    ['docs only', ['docs/a.md', 'website/x.mjs', 'README.md'], 'false', 'false', '[]'],
    ['web only', ['web/src/a.ts', 'web/AGENTS.md'], 'true', 'true', '["web-e2e"]'],
    ['Rust only', ['src/a.rs'], 'false', 'true', ALL],
    ['Rust and web', ['src/a.rs', 'web/src/a.ts'], 'true', 'true', ALL],
    ['workflow edit', ['.github/workflows/tests.yml'], 'true', 'true', ALL],
    ['rename out of web', [{ filename: 'src/a.rs', previous_filename: 'web/a.ts' }], 'true', 'true', ALL],
    ['rename into docs', [{ filename: 'docs/a.md', previous_filename: 'src/a.rs' }], 'false', 'true', ALL],
  ];
  for (const [name, files, web, live, legs] of cases) {
    const out = await decide({ files });
    assert.deepEqual([out.web, out.live, out.legs], [web, live, legs], name);
    const skipped = JSON.parse(out.skipped_legs);
    assert.deepEqual([...JSON.parse(legs), ...skipped].sort(), JSON.parse(ALL).sort(), `${name}: legs partition`);
  }
});

test('a release PR reports every leg as skipped, a fork release branch does not', async () => {
  assert.equal((await decide({ files: ['src/a.rs'], headRef: 'release-plz-1' })).skipped_legs, ALL);
  assert.equal((await decide({ files: ['src/a.rs'], headRef: 'release-plz-1', fork: true })).legs, ALL);
});

test('a diff too large to list runs everything', async () => {
  const files = Array.from({ length: 3000 }, (_, i) => `docs/${i}.md`);
  const out = await decide({ files });
  assert.deepEqual([out.web, out.live, out.legs], ['true', 'true', ALL]);
});
