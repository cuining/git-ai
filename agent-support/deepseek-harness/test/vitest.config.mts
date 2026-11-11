import { createRequire } from 'node:module'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = process.env.DSH_ROOT!
const requireDsh = createRequire(join(root, 'package.json'))
const ts = requireDsh('typescript')
const { standardDecoratorPlugin } = await import(/* @vite-ignore */ pathToFileURL(join(root, 'vitest.shared.ts')).href)
const paths = ts.readConfigFile(join(root, 'tsconfig.base.json'), ts.sys.readFile).config.compilerOptions.paths as Record<string, string[]>

function exact(name: string, replacement: string) {
  return { find: new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`), replacement }
}

const alias = Object.entries(paths).filter(([name]) => !name.includes('*'))
  .map(([name, targets]) => exact(name, resolve(root, targets[0]!)))
alias.push(exact('vitest', join(root, 'node_modules/vitest/dist/index.js')))
alias.push(exact('dsh-test/mock-adapter', join(root, 'packages/core/agent-loop/tests/mock-adapter.ts')))

export default {
  plugins: [standardDecoratorPlugin()],
  resolve: { alias },
  server: { fs: { allow: [root, resolve(import.meta.dirname, '..')] } },
  test: { dir: import.meta.dirname, include: ['*.spec.ts'], pool: 'forks', maxWorkers: 1, testTimeout: 30000 },
}
