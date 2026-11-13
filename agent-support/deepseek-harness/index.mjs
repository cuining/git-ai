import { isAbsolute, resolve } from 'node:path'

export const name = 'git-ai-deepseek-harness'
export const inject = ['fs', 'subprocess', 'tools', 'workingDirectory']

const gitAi = 'git-ai'

async function checkpoint(ctx, payload, signal) {
  const deadline = AbortSignal.timeout(3000)
  const child = ctx.subprocess.spawn({
    argv: [gitAi, 'checkpoint', 'agent-v1', '--hook-input', 'stdin', '--print-receipt'],
    cwd: payload.repo_working_dir,
    stdio: {
      stdin: { data: JSON.stringify(payload) },
      stdout: { maxBytes: 2048 },
      stderr: { maxBytes: 2048 },
    },
    graceMs: 200,
    signal: signal ? AbortSignal.any([deadline, signal]) : deadline,
  })
  const result = await child.done
  return result.exitCode === 0 &&
    /^checkpoint_requests=1\r?$/m.test(child.collected.stdout.readFrom(0).text)
}

async function canSnapshot(ctx, target, actor) {
  if (ctx.fs.sandboxMode === undefined) return true
  const policy = ctx.get('sandboxPolicy')?.resolve({ session: actor.agent.session })
  if (!policy || policy.mode === 'read-only') return false
  if (actor.arguments.sandbox_permissions && actor.arguments.sandbox_permissions !== policy.mode) return false
  if (policy.mode === 'danger-full-access') return true
  for (const root of [policy.workspaceRoot, ...policy.additionalWritableRoots ?? []]) {
    const resolvedRoot = await ctx.fs.resolve(root, { signal: actor.signal })
    if (ctx.fs.contains(resolvedRoot, target)) return true
  }
  return false
}

export function apply(ctx) {
  const calls = new WeakMap()
  const pendingByTarget = new Map()

  const capture = async (target, actor, next) => {
    const intent = await next()
    const call = calls.get(actor)
    if (!call || call.release) return intent
    try {
      const path = ctx.fs.processPath(target)
      if (!isAbsolute(path) || ctx.fs.processPathFromHostPath(path) !== resolve(path)) return intent
      if (!await canSnapshot(ctx, target, actor)) return intent
      const cwd = ctx.workingDirectory.get(actor.agent.session)
      if (!isAbsolute(cwd)) return intent

      // Hold the same target through its post-edit checkpoint, including when it already had edits.
      const key = target.targetKey
      const previous = pendingByTarget.get(key)
      const { promise, resolve: release } = Promise.withResolvers()
      const tail = previous ? previous.then(() => promise) : promise
      pendingByTarget.set(key, tail)
      call.release = () => {
        release()
        if (pendingByTarget.get(key) === tail) pendingByTarget.delete(key)
      }
      await previous
      if (actor.signal.aborted) return intent

      if (await checkpoint(ctx, {
        type: 'human',
        repo_working_dir: cwd,
        will_edit_filepaths: [path],
      }, actor.signal)) {
        call.snapshot = { cwd, key, path }
      }
    } catch {
      // Attribution is best-effort. The mutation keeps its original intent and outcome.
    }
    return intent
  }
  ctx.on('fs/write-intent', capture, { prepend: true })
  ctx.on('fs/edit-intent', capture, { prepend: true })
  ctx.on('fs/observed', (target, observation, actor) => {
    const call = calls.get(actor)
    if (call?.snapshot?.key === target.targetKey && observation.kind === 'present') {
      call.wrote = true
    }
  })
  ctx.on('tools/execute', async (exec, next) => {
    if (!exec.agent || (exec.name !== 'write' && exec.name !== 'edit')) return next()

    const call = {}
    calls.set(exec, call)
    try {
      return await next()
    } finally {
      calls.delete(exec)
      try {
        if (call.wrote) {
          const session = exec.agent.session
          const model = session.requestContext()?.model ?? exec.agent.options?.model
          await checkpoint(ctx, {
            type: 'ai_agent',
            repo_working_dir: call.snapshot.cwd,
            edited_filepaths: [call.snapshot.path],
            agent_name: 'deepseek-harness',
            conversation_id: session.id,
            model: typeof model === 'string' && model.length > 0 ? model : 'unknown',
          })
        }
      } catch {
        // A checkpoint failure must not change the tool result.
      } finally {
        call.release?.()
      }
    }
  })
}
