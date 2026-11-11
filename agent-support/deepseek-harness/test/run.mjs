import { spawnSync } from 'node:child_process'
import { realpathSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

if (!process.env.DSH_ROOT) throw new Error('Set DSH_ROOT to a DeepSeek Harness checkout with dependencies installed')
const root = realpathSync(process.env.DSH_ROOT)
const result = spawnSync(process.execPath, [
  join(root, 'node_modules/vitest/vitest.mjs'), 'run', '--configLoader', 'runner', '--config',
  fileURLToPath(new URL('./vitest.config.mts', import.meta.url)),
], {
  cwd: root,
  env: { ...process.env, DSH_ROOT: root },
  stdio: 'inherit',
})
if (result.error) throw result.error
process.exitCode = result.status ?? 1
