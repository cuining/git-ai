import { realpath } from 'node:fs'
import { lstat } from 'node:fs/promises'
import { isAbsolute, join, relative, resolve, sep } from 'node:path'
import { promisify } from 'node:util'

export const name = 'git-ai-attribution'
export const inject = ['fs', 'subprocess', 'tools', 'workingDirectory']

const canonicalPath = promisify(realpath.native)
const CHECKPOINT_ARGS = ['checkpoint', 'agent-v1', '--hook-input', 'stdin']
const CHECKPOINT_ERROR = /preset error|background worker unavailable|failed to send checkpoint|usage: git-ai checkpoint/i

export function apply(ctx, {
  gitAiPath,
  timeoutMs = 3000,
  captureBash = false,
} = {}) {
  if (gitAiPath !== undefined && (typeof gitAiPath !== 'string' || !gitAiPath.trim()
    || (!isAbsolute(gitAiPath) && /[/\\]/.test(gitAiPath)))) {
    throw new TypeError('git-ai-attribution requires an absolute gitAiPath or a command name')
  }
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 2 ** 31 - 1) {
    throw new TypeError('git-ai-attribution requires a positive timeoutMs within the timer limit')
  }
  if (typeof captureBash !== 'boolean') {
    throw new TypeError('git-ai-attribution requires a boolean captureBash')
  }

  const lifetime = new AbortController()
  const fileCalls = new WeakMap()
  const fileTails = new Map()
  let executable
  ctx.effect(() => () => lifetime.abort())

  function report(message) {
    if (!lifetime.signal.aborted) ctx.logger.warn(`git-ai-attribution: ${message}`)
  }

  async function checkpoint(payload, callerSignal) {
    const timeout = AbortSignal.timeout(timeoutMs)
    const signals = [lifetime.signal, timeout]
    if (callerSignal) signals.push(callerSignal)
    const signal = AbortSignal.any(signals)
    if (signal.aborted) return false
    try {
      executable ??= await ctx.subprocess.resolveExecutable(gitAiPath ?? 'git-ai', undefined, signal)
      const handle = ctx.subprocess.spawn({
        argv: [executable, ...CHECKPOINT_ARGS],
        cwd: payload.repo_working_dir,
        stdio: {
          stdin: { data: JSON.stringify(payload) },
          stdout: { maxBytes: 2048 },
          stderr: { maxBytes: 2048 },
        },
        graceMs: 250,
        signal,
        // The subprocess provider strips credential-shaped GIT_CONFIG_KEY_* names.
        env: { GIT_CONFIG_COUNT: '0', GIT_TERMINAL_PROMPT: '0' },
      })
      const outcome = await handle.done
      const stderr = handle.collected.stderr?.readFrom(0).text ?? ''
      if (!signal.aborted && outcome.exitCode === 0 && !CHECKPOINT_ERROR.test(stderr)) return true
    } catch {
      // Collection is best-effort and should not change the tool's outcome.
    }
    if (!callerSignal?.aborted) report(`${payload.type} checkpoint failed or timed out`)
    return false
  }

  function identity(agent) {
    return {
      agent_name: 'deepseek-harness',
      model: agent.session.requestContext()?.model ?? agent.options.model ?? 'unknown',
      conversation_id: agent.session.id,
    }
  }

  function isLocal(path) {
    return isAbsolute(path) && ctx.fs.processPathFromHostPath(path) === resolve(path)
  }

  async function permitsCapture(exec, path, defaultMode, wholeWorktree = false) {
    if (!isLocal(path)) return false
    if (defaultMode === undefined) return true
    if (exec.arguments?.sandbox_permissions !== undefined) return false

    // Checkpoints run on the host. Stay within the current tool's write scope.
    const policy = ctx.get('sandboxPolicy')?.resolve({ session: exec.agent.session })
    if (policy?.mode === 'danger-full-access') return true
    if (policy?.mode !== 'workspace-write') return false
    const fromRoot = relative(await canonicalPath(policy.workspaceRoot), path)
    if (isAbsolute(fromRoot) || fromRoot === '..' || fromRoot.startsWith(`..${sep}`)) return false
    if (!wholeWorktree) return true
    // Bash can snapshot the whole repository. A .git marker here keeps the
    // snapshot rooted inside the workspace instead of discovering an ancestor.
    const marker = await lstat(join(path, '.git')).catch(() => undefined)
    return marker?.isDirectory() || marker?.isFile()
  }

  async function waitForFile(path, call) {
    const previous = fileTails.get(path)
    const { promise, resolve } = Promise.withResolvers()
    fileTails.set(path, promise)
    call.release = () => {
      resolve()
      if (fileTails.get(path) === promise) fileTails.delete(path)
    }
    // Keep each pair ordered even when different sessions edit the same path.
    await previous
  }

  async function captureFileIntent(target, actor, next) {
    const intent = await next()
    const call = fileCalls.get(actor)
    if (!call) return intent
    try {
      const path = ctx.fs.processPath(target)
      if (!isLocal(path)) return intent
      const cwd = await ctx.workingDirectory.ensure(actor.agent, actor.signal)
      if (!isLocal(cwd)) return intent
      await waitForFile(path, call)
      if (!await permitsCapture(actor, path, ctx.fs.sandboxMode)) return intent
      const prepared = await checkpoint({
        type: 'human',
        repo_working_dir: cwd,
        will_edit_filepaths: [path],
      }, actor.signal)
      if (prepared) Object.assign(call, { cwd, path, targetKey: target.targetKey })
    } catch {
      report('file checkpoint target unavailable')
    }
    return intent
  }

  ctx.on('fs/write-intent', captureFileIntent, { prepend: true })
  ctx.on('fs/edit-intent', captureFileIntent, { prepend: true })
  ctx.on('fs/observed', (target, observation, actor) => {
    const call = fileCalls.get(actor)
    if (call?.path && call.targetKey === target.targetKey && observation.kind === 'present') {
      call.written = true
    }
  })

  async function captureBashCall(exec, next) {
    const args = exec.arguments
    const shell = ctx.get('shell')
    const definition = ctx.tools.get('bash', exec.agent)
    // A background-enabled registration may also promote foreground calls.
    if (!shell || !definition || Object.hasOwn(definition.parameters, 'run_in_background')
      || typeof args?.command !== 'string' || args.run_in_background === true
      || args.sandbox_permissions !== undefined) return next()

    let cwd
    try {
      const sessionCwd = await ctx.workingDirectory.ensure(exec.agent, exec.signal)
      if (!isLocal(sessionCwd)) return next()
      const workdir = typeof args.workdir !== 'string' ? sessionCwd
        : isAbsolute(args.workdir) ? args.workdir : `${sessionCwd}${sep}${args.workdir}`
      const spec = shell.resolve({ command: args.command, workdir, timeoutMs: args.timeoutMs })
      if (!isLocal(spec.workdir)) return next()
      cwd = await canonicalPath(spec.workdir)
      if (!await permitsCapture(exec, cwd, shell.sandboxMode, true)) return next()
    } catch {
      report('Bash checkpoint working directory unavailable')
      return next()
    }

    const payload = {
      repo_working_dir: cwd,
      ...identity(exec.agent),
      tool_use_id: exec.callId,
    }
    const prepared = await checkpoint({ type: 'pre_shell_command', ...payload }, exec.signal)
    const result = await next()
    if (prepared && !result.isError && result.value?.kind === 'foreground'
      && !result.value.sandbox?.denied) {
      await checkpoint({ type: 'post_shell_command', ...payload })
    }
    return result
  }

  ctx.on('tools/execute', async (exec, next) => {
    if (!exec.agent || exec.signal.aborted) return next()
    if (exec.name === 'bash' && captureBash) return captureBashCall(exec, next)
    if (exec.name !== 'write' && exec.name !== 'edit') return next()

    const call = { identity: identity(exec.agent) }
    fileCalls.set(exec, call)
    try {
      return await next()
    } finally {
      fileCalls.delete(exec)
      try {
        if (call.written) {
          await checkpoint({
            type: 'ai_agent',
            repo_working_dir: call.cwd,
            edited_filepaths: [call.path],
            ...call.identity,
          })
        }
      } finally {
        call.release?.()
      }
    }
  })
}
