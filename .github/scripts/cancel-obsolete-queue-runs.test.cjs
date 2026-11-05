const { test } = require('node:test');
const assert = require('node:assert/strict');
const cancel = require('./cancel-obsolete-queue-runs.cjs');

const branch = (pr = 42, base = 'main', sha = 'a') =>
  `gh-readonly-queue/${base}/pr-${pr}-${sha.repeat(40)}`;
const current = {
  id: 20, run_number: 10, run_attempt: 1, workflow_id: 100, event: 'merge_group',
  path: '.github/workflows/ci.yml', head_branch: branch(), head_sha: 'b'.repeat(40),
  repository: { id: 1 }, head_repository: { id: 1 }, status: 'queued',
};
const old = { ...current, id: 19, run_number: 9, head_branch: branch(42, 'main', 'c'), head_sha: 'd'.repeat(40) };

async function exercise({ trigger = {}, candidates = [old], fresh = {}, queueSha = current.head_sha, queueBase = 'main',
  queueError, cancelError, latest, pageError, leaveQueue = false, advanceQueue = () => false, event = current } = {}) {
  const run = { ...current, ...trigger };
  const cancelled = [];
  const queries = [];
  const reads = [];
  let queueReads = 0;
  const github = {
    rest: {
      actions: {
        listWorkflowRuns: () => {},
        getWorkflowRun: async ({ run_id }) => {
          reads.push(run_id);
          if (run_id === run.id) return { data: run };
          const candidate = candidates.find(c => c.id === run_id);
          return { data: latest && reads.filter(id => id === run_id).length > 1
            ? { ...candidate, ...latest } : { ...candidate, ...fresh } };
        },
        cancelWorkflowRun: async ({ run_id }) => {
          assert.equal(queries.length, 5, 'snapshot all statuses before mutating');
          cancelled.push(run_id);
          if (cancelError) throw cancelError;
        },
      },
    },
    graphql: async (query, args) => {
      assert.equal(args.number, 42);
      assert.match(query, /mergeQueueEntry/);
      queueReads++;
      if (queueError) throw queueError;
      return { repository: { pullRequest: {
        baseRefName: queueBase,
        mergeQueueEntry: leaveQueue ? null : {
          headCommit: { oid: advanceQueue({ reads: queueReads, cancelled }) ? 'new-generation' : queueSha },
        },
      } } };
    },
    paginate: async (method, args) => {
      assert.equal(method, github.rest.actions.listWorkflowRuns);
      assert.equal(args.event, 'merge_group');
      assert.equal(args.workflow_id, run.workflow_id);
      assert.equal(args.per_page, 100);
      queries.push(args.status);
      if (pageError) throw pageError;
      return candidates.filter(c => c.status === args.status);
    },
  };
  await cancel({ github, context: {
    repo: { owner: 'owner', repo: 'repo' },
    payload: { repository: { id: 1 }, workflow_run: event },
  }, core: { notice() {} } });
  return { cancelled, queries, reads };
}

test('cancels obsolete CI and Tests generations, including more than 100 returned candidates', async () => {
  for (const path of ['.github/workflows/ci.yml', '.github/workflows/tests.yml']) {
    const candidates = Array.from({ length: 105 }, (_, i) => ({ ...old, path, id: 1000 + i }));
    candidates.push({ ...old, path, id: 2000, status: 'in_progress' });
    const result = await exercise({ trigger: { path }, event: { ...current, path }, candidates });
    assert.deepEqual(result.cancelled, candidates.map(c => c.id));
    assert.deepEqual(result.queries, ['queued', 'in_progress', 'waiting', 'pending', 'requested']);
  }
});

test('leaves unrelated, newer, completed and same-SHA runs alone', async () => {
  const cases = [
    { head_branch: branch(43) }, { head_branch: branch(42, 'release/main') },
    { head_branch: 'feature/fake-queue' }, { head_branch: branch() + '-suffix' },
    { event: 'pull_request' }, { workflow_id: 101 },
    { path: '.github/workflows/tests.yml' }, { repository: { id: 2 } },
    { head_repository: { id: 2 } }, { run_number: 10 }, { run_number: 11 },
    { head_sha: current.head_sha }, { status: 'completed' }, { run_attempt: 2 },
  ];
  for (const change of cases) {
    assert.deepEqual((await exercise({ candidates: [{ ...old, ...change }] })).cancelled, [], JSON.stringify(change));
  }
});

test('rejects untrusted triggers and generations no longer current in the queue', async () => {
  for (const trigger of [
    { event: 'pull_request' }, { path: '.github/workflows/release.yml' },
    { head_branch: 'main' }, { head_sha: 'changed' }, { workflow_id: 101 },
    { repository: { id: 2 } }, { head_repository: { id: 2 } },
  ]) {
    assert.deepEqual((await exercise({ trigger })).queries, [], JSON.stringify(trigger));
  }
  assert.deepEqual((await exercise({ queueSha: 'changed' })).cancelled, []);
  assert.deepEqual((await exercise({ queueBase: 'release' })).cancelled, []);
  assert.deepEqual((await exercise({ leaveQueue: true })).cancelled, []);
  assert.deepEqual((await exercise({ advanceQueue: ({ reads }) => reads > 1 })).cancelled, []);
  await assert.rejects(exercise({ queueError: new Error('API failure') }), /API failure/);
});

test('stops cancelling remaining candidates when the queue advances after a cancellation', async () => {
  const result = await exercise({
    candidates: [old, { ...old, id: 18, run_number: 8 }],
    advanceQueue: ({ cancelled }) => cancelled.length > 0,
  });
  assert.deepEqual(result.cancelled, [old.id]);
});

test('a delayed old generation cannot cancel the current group despite a higher run number', async () => {
  const delayed = { ...current, run_number: 11, head_sha: old.head_sha };
  const result = await exercise({ trigger: delayed, event: delayed,
    candidates: [{ ...old, run_number: 10, head_sha: current.head_sha }],
  });
  assert.deepEqual(result.cancelled, []);
  assert.deepEqual(result.queries, []);
});

test('rechecks SHA, identity, ordering and completion before cancelling', async () => {
  for (const fresh of [
    { head_sha: 'changed' }, { status: 'completed' }, { event: 'push' },
    { head_branch: branch(43) }, { run_number: 11 }, { run_attempt: 2 },
    { head_repository: { id: 2 } },
  ]) {
    assert.deepEqual((await exercise({ fresh })).cancelled, [], JSON.stringify(fresh));
  }
});

test('accepts completion races but propagates API and cancellation failures', async () => {
  await exercise({ cancelError: { status: 409 }, latest: { status: 'completed' } });
  await assert.rejects(exercise({ cancelError: new Error('forbidden') }), /forbidden/);
  await assert.rejects(exercise({ cancelError: { status: 409 } }));
  await assert.rejects(exercise({ pageError: new Error('pagination failed') }), /pagination failed/);
});
