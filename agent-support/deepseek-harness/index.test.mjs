import assert from 'node:assert/strict'
import { resolve } from 'node:path'
import test from 'node:test'
import { apply } from './index.mjs'

function fixture({ acknowledge = true, sandboxMode, policy, events, failObservation } = {}) {
  const root = resolve('workspace')
  const target = { targetKey: 'target', path: resolve(root, 'file.txt') }
  const callbacks = new Map()
  const checkpoints = []
  let observationsDisposed = 0
  const ctx = {
    on(event, callback) { callbacks.set(event, callback) },
    get(service) {
      assert.equal(service, 'sandboxPolicy')
      return { resolve: () => policy }
    },
    fs: {
      sandboxMode,
      processPath: target => target.path,
      processPathFromHostPath: path => path,
      resolve: async path => ({ path }),
      contains: (parent, child) => child.path.startsWith(parent.path),
    },
    workingDirectory: { get: () => root },
    sessionQuery: {
      async observeSession(sessionId) {
        assert.equal(sessionId, 'session-a')
        if (failObservation) throw new Error('Session unavailable')
        return { events: events ?? [], [Symbol.dispose]: () => { observationsDisposed++ } }
      },
    },
    subprocess: {
      spawn({ argv, cwd, stdio }) {
        assert.equal(cwd, root)
        assert.deepEqual(argv, ['git-ai', 'checkpoint', 'agent-v1', '--hook-input', 'stdin', '--print-receipt'])
        const payload = JSON.parse(stdio.stdin.data)
        checkpoints.push(payload)
        return {
          done: Promise.resolve({ exitCode: 0 }),
          collected: { stdout: { readFrom: () => ({
            text: acknowledge ? 'checkpoint_requests=1\r\n' : '',
          }) } },
        }
      },
    },
  }
  apply(ctx)
  const run = async (name, body) => {
    const exec = {
      name, arguments: {}, signal: new AbortController().signal,
      agent: { session: { id: 'session-a', requestContext: () => ({ model: 'deepseek-test' }) } },
    }
    return callbacks.get('tools/execute')(exec, () => body({
      intent: (target = ctx.target) => callbacks.get(`fs/${name}-intent`)(target, exec, () => 'original intent'),
      observed: (target = ctx.target) => callbacks.get('fs/observed')(target, { kind: 'present' }, exec),
    }))
  }
  ctx.target = target
  return { checkpoints, target, root, run, get observationsDisposed() { return observationsDisposed } }
}

test('successful write and edit retain tool results and checkpoint only their observed target', async () => {
  const { checkpoints, root, target, run } = fixture()
  for (const name of ['write', 'edit']) {
    const result = await run(name, async ({ intent, observed }) => {
      assert.equal(await intent(), 'original intent')
      assert.equal(checkpoints.at(-1).type, 'human')
      observed()
      return 'original result'
    })
    assert.equal(result, 'original result')
    assert.deepEqual(checkpoints.slice(-2), [
      { type: 'human', repo_working_dir: root, will_edit_filepaths: [target.path] },
      {
        type: 'ai_agent', repo_working_dir: root, edited_filepaths: [target.path],
        agent_name: 'deepseek-harness', model: 'deepseek-test', conversation_id: 'session-a',
      },
    ])
  }
})

test('failed writes and observations of another target have no AI checkpoint', async () => {
  const { checkpoints, target, run } = fixture()
  const error = new Error('write rejected')
  await assert.rejects(run('write', async ({ intent, observed }) => {
    await intent()
    observed({ ...target, targetKey: 'other target' })
    throw error
  }), error)
  assert.deepEqual(checkpoints.map(entry => entry.type), ['human'])
})

test('an unacknowledged baseline lets the edit finish and skips AI attribution', async () => {
  const { checkpoints, run } = fixture({ acknowledge: false })
  await run('edit', async ({ intent, observed }) => {
    await intent()
    observed()
  })
  assert.deepEqual(checkpoints.map(entry => entry.type), ['human'])
})

test('the same file stays serialized through its AI checkpoint', async () => {
  const { checkpoints, run } = fixture()
  let startFirst
  let finishFirst
  const firstStarted = new Promise(resolve => { startFirst = resolve })
  const firstCanFinish = new Promise(resolve => { finishFirst = resolve })
  const first = run('write', async ({ intent, observed }) => {
    await intent()
    startFirst()
    await firstCanFinish
    observed()
  })
  await firstStarted
  let secondReachedIntent
  const secondAtIntent = new Promise(resolve => { secondReachedIntent = resolve })
  const second = run('edit', async ({ intent, observed }) => {
    const pending = intent()
    secondReachedIntent()
    await pending
    observed()
  })
  await secondAtIntent
  assert.deepEqual(checkpoints.map(entry => entry.type), ['human'])
  finishFirst()
  await Promise.all([first, second])
  assert.deepEqual(checkpoints.map(entry => entry.type), ['human', 'ai_agent', 'human', 'ai_agent'])
})

test('a confined file outside the writable roots produces no checkpoint', async () => {
  const { checkpoints, run } = fixture({
    sandboxMode: 'workspace-write',
    policy: { mode: 'workspace-write', workspaceRoot: resolve('other') },
  })
  await run('write', async ({ intent, observed }) => {
    await intent()
    observed()
  })
  assert.deepEqual(checkpoints, [])
})

test('AI checkpoints carry the full visible session prefix with one entry per tool call', async () => {
  const time = Date.parse('2026-01-02T03:04:05Z')
  const timestamp = new Date(time).toISOString()
  const text = text => ({ type: 'text', text })
  const user = (kind, message) => ({
    type: 'user/message', surfaceOp: 'append', time,
    data: { source: { kind }, content: [text(message)] },
  })
  const call = { type: 'tool/call', time, data: { name: 'write', arguments: '{"file_path":"file.txt"}' } }
  const events = [
    user('user', 'First prompt'),
    user('runtime-context', 'Injected context'),
    {
      type: 'assistant/message', surfaceOp: 'append', time,
      data: { message: { content: [
        text('Visible reply'), { type: 'reasoning', text: 'Private reasoning' },
        { type: 'tool-call', name: 'write', arguments: call.data.arguments },
      ] } },
    },
    call,
    { type: 'tool/result', surfaceOp: 'append', time, data: { content: [text('Tool output')] } },
  ]
  const trial = fixture({ events })
  const edit = ({ intent, observed }) => intent().then(() => observed())
  await trial.run('write', edit)
  const first = [
    { type: 'user', text: 'First prompt', timestamp },
    { type: 'assistant', text: 'Visible reply', timestamp },
    { type: 'tool_use', name: 'write', input: { file_path: 'file.txt' }, timestamp },
  ]
  assert.deepEqual(trial.checkpoints.at(-1).transcript.messages, first)
  events.push(user('user', 'Second prompt'))
  await trial.run('edit', edit)
  assert.deepEqual(trial.checkpoints.at(-1).transcript.messages, [
    ...first, { type: 'user', text: 'Second prompt', timestamp },
  ])
  assert.equal(trial.observationsDisposed, 2)
})

test('session query failure preserves the AI line checkpoint', async () => {
  const { run, checkpoints } = fixture({ failObservation: true })
  await run('write', async ({ intent, observed }) => {
    await intent()
    observed()
  })
  assert.deepEqual(checkpoints.map(entry => entry.type), ['human', 'ai_agent'])
  assert.equal(checkpoints.at(-1).transcript, undefined)
})
