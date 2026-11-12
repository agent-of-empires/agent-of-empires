const paths = new Set(['.github/workflows/ci.yml', '.github/workflows/tests.yml']);
const active = ['queued', 'in_progress', 'waiting', 'pending', 'requested'];

function entry(branch) {
  const match = /^gh-readonly-queue\/(.+)\/pr-([1-9]\d*)-[0-9a-f]{40}$/.exec(branch ?? '');
  return match && { base: match[1], number: Number(match[2]) };
}

function sameWorkflow(run, current, repositoryId) {
  return run.event === 'merge_group' && paths.has(run.path) &&
    run.path === current.path && run.workflow_id === current.workflow_id &&
    run.repository?.id === repositoryId && run.head_repository?.id === repositoryId;
}

module.exports = async ({ github, context, core }) => {
  const event = context.payload.workflow_run;
  const repositoryId = context.payload.repository.id;
  const { data: current } = await github.rest.actions.getWorkflowRun({
    ...context.repo, run_id: event.id,
  });
  const key = entry(current.head_branch);
  if (!key || !sameWorkflow(current, current, repositoryId) ||
      current.head_sha !== event.head_sha || current.workflow_id !== event.workflow_id) return;

  // Queue state, rather than event delivery order, identifies the current generation.
  const isCurrent = async () => {
    const { repository } = await github.graphql(`
      query($owner: String!, $repo: String!, $number: Int!) {
        repository(owner: $owner, name: $repo) {
          pullRequest(number: $number) {
            baseRefName
            mergeQueueEntry { headCommit { oid } }
          }
        }
      }`, { ...context.repo, number: key.number });
    const pr = repository?.pullRequest;
    return pr?.baseRefName === key.base &&
      pr.mergeQueueEntry?.headCommit?.oid === current.head_sha;
  };
  if (!await isCurrent()) return;

  const obsolete = run => {
    const candidate = entry(run.head_branch);
    return sameWorkflow(run, current, repositoryId) &&
      candidate?.base === key.base && candidate.number === key.number &&
      run.run_number < current.run_number && run.head_sha !== current.head_sha &&
      run.run_attempt === 1 && active.includes(run.status);
  };
  // Snapshot every page before cancelling: mutations can shift API pagination.
  const candidates = new Map();
  for (const status of active) {
    const runs = await github.paginate(github.rest.actions.listWorkflowRuns, {
      ...context.repo, workflow_id: current.workflow_id,
      event: 'merge_group', status, per_page: 100,
    });
    for (const run of runs) if (obsolete(run)) candidates.set(run.id, run);
  }
  for (const candidate of candidates.values()) {
    const { data: run } = await github.rest.actions.getWorkflowRun({
      ...context.repo, run_id: candidate.id,
    });
    if (!obsolete(run) || run.head_sha !== candidate.head_sha) continue;
    if (!await isCurrent()) return;
    try {
      await github.rest.actions.cancelWorkflowRun({ ...context.repo, run_id: run.id });
      core.notice(`Cancelled obsolete queue run ${run.id} for ${key.base}/${key.number}`);
    } catch (error) {
      if (error.status !== 409) throw error;
      const { data: latest } = await github.rest.actions.getWorkflowRun({
        ...context.repo, run_id: run.id,
      });
      if (latest.status !== 'completed') throw error;
    }
  }
};
