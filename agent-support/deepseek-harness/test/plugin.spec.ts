import { mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'
import { expect, it, vi } from 'vitest'
import { Context } from '@deepseek-ai/cordis'
import { mountAgentLoopTestDependencies, mountAgentLoopTestHarness } from '@deepseek-ai/dsh-agent-loop-testkit'
import { createUserMessage, ToolCallId } from '@deepseek-ai/dsh-llm'
import { SessionId } from '@deepseek-ai/dsh-session'
import LocalSubprocessRuntime from '@deepseek-ai/dsh-subprocess-local'
import { LocalBashExecutor } from '@deepseek-ai/dsh-bash-local'
import { SandboxedFileSystem } from '@deepseek-ai/dsh-fs-sandbox'
import SandboxPolicyService from '@deepseek-ai/dsh-sandbox-policy'
import type { SandboxMode } from '@deepseek-ai/dsh-sandbox'
import WorkingDirectory from '@deepseek-ai/dsh-working-directory'
import * as BashEnv from '@deepseek-ai/dsh-shell-env'
import * as ToolBash from '@deepseek-ai/dsh-tool-bash'
import * as ToolFs from '@deepseek-ai/dsh-tool-fs'
import { MockAdapter, textResponse, toolCallResponse } from 'dsh-test/mock-adapter'
import * as GitAi from '../git-ai.mjs'

const untouched = 'unchanged one\nunchanged two\nunchanged three\nunchanged four\nhuman pending\n'
const bash = `python3 -c "from pathlib import Path; p = Path('dsh-bash.txt'); p.write_text(p.read_text() + 'AI bash\\n')"`
const session = process.env.DSH_GIT_AI_SESSION ?? 'dsh-git-ai-e2e'
const identity = { agent_name: 'deepseek-harness', model: 'poc-model', conversation_id: session }

async function mountHarness(
  ctx: Context,
  scratch: string,
  gitAiPath: string | undefined,
  { timeoutMs = 5000, sandboxMode }: { timeoutMs?: number, sandboxMode?: SandboxMode } = {},
) {
  await mountAgentLoopTestDependencies(ctx, { workingDirectory: sandboxMode === undefined })
  if (sandboxMode !== undefined) {
    await ctx.plugin(SandboxPolicyService, { mode: sandboxMode })
    await ctx.plugin(SandboxedFileSystem)
    await ctx.plugin(WorkingDirectory)
  }
  const driver = await mountAgentLoopTestHarness(ctx)
  await ctx.plugin(LocalSubprocessRuntime)
  await ctx.plugin(BashEnv, { dshHome: scratch })
  await ctx.plugin(LocalBashExecutor, { timeoutMs: 10000 })
  await ctx.plugin(ToolFs)
  await ctx.plugin(ToolBash, { enableRunInBackground: false, promoteOnTimeout: false })
  await ctx.plugin(GitAi, { gitAiPath, timeoutMs, captureBash: true })
  return driver
}

async function runTools(ctx: Context, driver: Awaited<ReturnType<typeof mountHarness>>, root: string, responses: ReturnType<typeof toolCallResponse>[]) {
  ctx.llm.registerAdapter(['mock'], new MockAdapter([...responses, textResponse('done')]))
  const agent = await driver.create(SessionId(session), { provider: 'mock', model: 'poc-model' }, { cwd: root })
  agent.followup(createUserMessage({ content: [{ type: 'text', text: 'exercise real tools' }], source: { kind: 'user' } }))
  await agent.whenIdle()
  return agent.session.snapshotEvents().filter(event => event.type === 'tool/result')
}

async function readCheckpoints(log: string) {
  return (await readFile(log, 'utf8')).split('\n').filter(Boolean).map(line => JSON.parse(line))
}

async function recordingExecutable(scratch: string, log: string) {
  const file = join(scratch, 'git-ai.mjs')
  await writeFile(file, `#!/usr/bin/env node
import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { basename, join } from 'node:path'
const payload = JSON.parse(readFileSync(0, 'utf8'))
const cwd = process.cwd()
const target = payload.will_edit_filepaths?.[0] ?? payload.edited_filepaths?.[0] ?? join(cwd, 'dsh-bash.txt')
let content = null
try { content = readFileSync(target, 'utf8') } catch (error) { if (error.code !== 'ENOENT') throw error }
const log = ${JSON.stringify(log)}
const holdFirst = payload.type === 'human' && basename(target) === 'shared.txt' && !readFileSync(log, 'utf8')
appendFileSync(log, JSON.stringify({ argv: process.argv.slice(2), cwd, payload, content }) + '\\n')
if (payload.type === 'human' && basename(target) === 'failed-checkpoint.txt') process.exitCode = 1
if (payload.type === 'human' && basename(target) === 'timed-out-checkpoint.txt') setInterval(() => {}, 1000)
if (holdFirst) {
  const timer = setInterval(() => { if (existsSync(join(cwd, 'release-first'))) clearInterval(timer) }, 10)
}
`, { mode: 0o700 })
  return file
}

it('captures actual file changes and foreground shell writes in their respective working directories', async () => {
  const scratch = await realpath(await mkdtemp(join(tmpdir(), 'dsh-git-ai-native-')))
  const ctx = new Context()
  try {
    const workspace = process.env.DSH_GIT_AI_WORKSPACE
    const binary = process.env.DSH_GIT_AI_BIN
    if (Boolean(workspace) !== Boolean(binary)) throw new Error('Set both DSH_GIT_AI_WORKSPACE and DSH_GIT_AI_BIN for the real git-ai case')
    const root = workspace ? await realpath(workspace) : scratch
    const childA = join(root, 'child-a')
    const childB = join(root, 'child-b')
    const written = join(childA, 'dsh-write.txt')
    const edited = join(childA, 'dsh-edit.txt')
    const scripted = join(childB, 'dsh-bash.txt')
    const log = join(scratch, 'checkpoint.jsonl')
    if (!workspace) {
      await Promise.all([mkdir(childA), mkdir(childB)])
      await writeFile(edited, `base edit\n${untouched}`)
      await writeFile(scripted, 'base bash\n')
      await writeFile(log, '')
    }

    const driver = await mountHarness(ctx, scratch, binary ?? await recordingExecutable(scratch, log))
    const results = await runTools(ctx, driver, root, [
      toolCallResponse('dsh-write', 'write', { file_path: 'child-a/dsh-write.txt', content: 'AI write\n' }),
      toolCallResponse('dsh-edit', 'edit', { file_path: 'child-a/dsh-edit.txt', old_string: 'base edit', new_string: 'AI edit' }),
      toolCallResponse('dsh-bash', 'bash', { command: bash, description: 'append a line using Python', workdir: 'child-b' }),
    ])
    expect(results.map(event => event.data.message.isError)).toEqual([false, false, false])
    expect(await readFile(written, 'utf8')).toBe('AI write\n')
    expect(await readFile(edited, 'utf8')).toBe(`AI edit\n${untouched}`)
    expect(await readFile(scripted, 'utf8')).toBe('base bash\nAI bash\n')

    if (!binary) {
      const records = await readCheckpoints(log)
      const argv = ['checkpoint', 'agent-v1', '--hook-input', 'stdin']
      expect(records).toMatchObject([
        { argv, cwd: root, payload: { type: 'human', repo_working_dir: root, will_edit_filepaths: [written] }, content: null },
        { argv, cwd: root, payload: { type: 'ai_agent', repo_working_dir: root, edited_filepaths: [written], ...identity }, content: 'AI write\n' },
        { argv, cwd: root, payload: { type: 'human', repo_working_dir: root, will_edit_filepaths: [edited] }, content: `base edit\n${untouched}` },
        { argv, cwd: root, payload: { type: 'ai_agent', repo_working_dir: root, edited_filepaths: [edited], ...identity }, content: `AI edit\n${untouched}` },
        { argv, cwd: childB, payload: { type: 'pre_shell_command', repo_working_dir: childB, tool_use_id: 'dsh-bash', ...identity }, content: 'base bash\n' },
        { argv, cwd: childB, payload: { type: 'post_shell_command', repo_working_dir: childB, tool_use_id: 'dsh-bash', ...identity }, content: 'base bash\nAI bash\n' },
      ])
      expect(records.slice(-2).map(record => record.payload.command)).toEqual([undefined, undefined])
    }
  } finally {
    await ctx.fiber.dispose()
    await rm(scratch, { recursive: true, force: true })
  }
})

it.skipIf(Boolean(process.env.DSH_GIT_AI_BIN)).each([true, false])(
  'uses only the default dcc-ai-tracking command when present=%s', async (available) => {
    const root = await realpath(await mkdtemp(join(tmpdir(), 'dsh-git-ai-path-')))
    const ctx = new Context()
    try {
      const canonicalLog = join(root, 'dcc-ai-tracking.jsonl')
      const legacyLog = join(root, 'git-ai.jsonl')
      await Promise.all([writeFile(canonicalLog, ''), writeFile(legacyLog, '')])
      for (const name of available ? ['git-ai', 'dcc-ai-tracking'] : ['git-ai']) {
        const dir = join(root, `${name}-fixture`)
        await mkdir(dir)
        await symlink(await recordingExecutable(dir, join(root, `${name}.jsonl`)), join(root, name))
      }
      const driver = await mountHarness(ctx, root, undefined)
      const resolveExecutable = ctx.subprocess.resolveExecutable.bind(ctx.subprocess)
      vi.spyOn(ctx.subprocess, 'resolveExecutable').mockImplementation((name, env, signal) =>
        resolveExecutable(name, { ...env, PATH: root }, signal))

      const file = join(root, 'file.txt')
      const [result] = await runTools(ctx, driver, root, [
        toolCallResponse('default-path', 'write', { file_path: file, content: 'AI write\n' }),
      ])
      expect(result?.data.message.isError).toBe(false)
      expect((await readCheckpoints(canonicalLog)).map(record => record.payload.type))
        .toEqual(available ? ['human', 'ai_agent'] : [])
      expect(await readCheckpoints(legacyLog)).toEqual([])
    } finally {
      await ctx.fiber.dispose()
      await rm(root, { recursive: true, force: true })
    }
  },
)

it.skipIf(Boolean(process.env.DSH_GIT_AI_BIN))('preserves tool outcomes when checkpoints fail and only records completed mutations', async () => {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'dsh-git-ai-failure-')))
  const ctx = new Context()
  try {
    const failed = join(root, 'failed-checkpoint.txt')
    const timedOut = join(root, 'timed-out-checkpoint.txt')
    const denied = join(root, 'denied.txt')
    const failedEdit = join(root, 'failed-edit.txt')
    const scripted = join(root, 'dsh-bash.txt')
    const log = join(root, 'checkpoint.jsonl')
    await writeFile(failedEdit, 'original\n')
    await writeFile(scripted, 'base bash\n')
    const driver = await mountHarness(ctx, root, await recordingExecutable(root, log), { timeoutMs: 1500 })
    ctx.on('tools/pre-execute', (execution, next) => execution.callId === 'denied'
      ? Promise.resolve({ kind: 'deny', reason: 'test refusal' })
      : next())
    const results = await runTools(ctx, driver, root, [
      toolCallResponse('fail', 'write', { file_path: failed, content: 'written after failure\n' }),
      toolCallResponse('timeout', 'write', { file_path: timedOut, content: 'written after timeout\n' }),
      toolCallResponse('denied', 'write', { file_path: denied, content: 'denied\n' }),
      toolCallResponse('edit', 'edit', { file_path: failedEdit, old_string: 'not present', new_string: 'other' }),
      toolCallResponse('nonzero', 'bash', { command: `${bash} && exit 7`, description: 'write before nonzero exit' }),
    ])

    expect(results.map(event => event.data.message.isError)).toEqual([false, false, true, true, false])
    expect(results[4]?.data.message.content).toContainEqual({ type: 'text', text: expect.stringContaining('[exit code: 7]') })
    expect(await readFile(failed, 'utf8')).toBe('written after failure\n')
    expect(await readFile(timedOut, 'utf8')).toBe('written after timeout\n')
    await expect(readFile(denied, 'utf8')).rejects.toMatchObject({ code: 'ENOENT' })
    expect(await readFile(failedEdit, 'utf8')).toBe('original\n')
    expect(await readFile(scripted, 'utf8')).toBe('base bash\nAI bash\n')
    const records = await readCheckpoints(log)
    expect(records.map(record => record.payload)).toMatchObject([
      { type: 'human', will_edit_filepaths: [failed] },
      { type: 'human', will_edit_filepaths: [timedOut] },
      { type: 'human', will_edit_filepaths: [failedEdit] },
      { type: 'pre_shell_command', tool_use_id: 'nonzero', ...identity },
      { type: 'post_shell_command', tool_use_id: 'nonzero', ...identity },
    ])
  } finally {
    await ctx.fiber.dispose()
    await rm(root, { recursive: true, force: true })
  }
})

it.skipIf(Boolean(process.env.DSH_GIT_AI_BIN)).each(['read-only', 'workspace-write'] as const)('respects %s file capture bounds', async (sandboxMode) => {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'dsh-git-ai-sandbox-')))
  const ctx = new Context()
  try {
    const file = join(root, 'file.txt')
    const log = join(root, 'checkpoint.jsonl')
    await writeFile(file, 'original\n')
    await writeFile(log, '')
    const driver = await mountHarness(ctx, root, await recordingExecutable(root, log), { sandboxMode })
    const [result] = await runTools(ctx, driver, root, [
      toolCallResponse('sandbox-write', 'write', { file_path: file, content: 'new content\n' }),
    ])
    const denied = sandboxMode === 'read-only'
    expect(result?.data.message.isError).toBe(denied)
    expect(await readFile(file, 'utf8')).toBe(denied ? 'original\n' : 'new content\n')
    expect((await readCheckpoints(log)).map(record => record.payload.type)).toEqual(denied ? [] : ['human', 'ai_agent'])
  } finally {
    await ctx.fiber.dispose()
    await rm(root, { recursive: true, force: true })
  }
})

it.skipIf(Boolean(process.env.DSH_GIT_AI_BIN))('keeps a same-path baseline and mutation together across sessions', async () => {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'dsh-git-ai-shared-')))
  const ctx = new Context()
  const release = join(root, 'release-first')
  try {
    const file = join(root, 'shared.txt')
    const log = join(root, 'checkpoint.jsonl')
    await writeFile(file, 'initial\n')
    await writeFile(log, '')
    const driver = await mountHarness(ctx, root, await recordingExecutable(root, log), { timeoutMs: 10000 })
    const first = await driver.create(SessionId('first-session'), {}, { cwd: root })
    const second = await driver.create(SessionId('second-session'), {}, { cwd: root })
    const write = (agent: typeof first, content: string) => ctx.tools.execute({
      name: 'write', agent, callId: ToolCallId(`write-${agent.id}`), signal: new AbortController().signal,
      arguments: { file_path: file, content },
    })
    const firstWrite = write(first, 'first edit\n')
    await expect.poll(async () => (await readCheckpoints(log)).length, { timeout: 5000 }).toBe(1)
    const secondWrite = write(second, 'second edit\n')
    // Allow the second write to reach collection while the first pre-checkpoint is held.
    await Promise.race([secondWrite, delay(1000)])
    await writeFile(release, '')
    expect((await Promise.all([firstWrite, secondWrite])).map(result => result.isError)).toEqual([false, false])
    expect(await readCheckpoints(log)).toMatchObject([
      { payload: { type: 'human' }, content: 'initial\n' },
      { payload: { type: 'ai_agent', conversation_id: 'first-session' }, content: 'first edit\n' },
      { payload: { type: 'human' }, content: 'first edit\n' },
      { payload: { type: 'ai_agent', conversation_id: 'second-session' }, content: 'second edit\n' },
    ])
    expect(await readFile(file, 'utf8')).toBe('second edit\n')
  } finally {
    await writeFile(release, '')
    await ctx.fiber.dispose()
    await rm(root, { recursive: true, force: true })
  }
})
