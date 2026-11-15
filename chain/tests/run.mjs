// Starts the local validator (hub_registry, demo_pool and the Acrux Net genesis accounts
// loaded), runs the test files against it, and stops it again.
import { spawn, spawnSync } from 'node:child_process'
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { Connection } from '@solana/web3.js'
import { chainDir } from '../scripts/dev-keys.mjs'
import { RPC_URL } from '../scripts/registry.mjs'

const state = join(chainDir, '.ledger')
const accounts = join(state, 'accounts')
const genesis = JSON.parse(readFileSync(join(chainDir, 'genesis/acrux-net.json'), 'utf8'))

rmSync(state, { recursive: true, force: true })
mkdirSync(accounts, { recursive: true })
for (const a of genesis.accounts) {
  writeFileSync(join(accounts, `${a.address}.json`), JSON.stringify({
    pubkey: a.address,
    account: { lamports: a.lamports, data: [a.data, 'base64'], owner: a.owner, executable: a.executable, rentEpoch: 0 },
  }))
}

const validator = spawn('bash', [
  join(chainDir, 'scripts/validator.sh'), state, '32',
  '--bpf-program', genesis.programs[0].address, join(chainDir, genesis.programs[0].path),
  '--account-dir', accounts,
], { stdio: 'ignore' })

let status = 1
try {
  const connection = new Connection(RPC_URL, 'confirmed')
  for (let i = 0; ; i++) {
    if (validator.exitCode !== null) throw new Error('The validator exited; see .ledger/validator.log')
    if ((await connection.getSlot().catch(() => 0)) > 1) break
    if (i > 120) throw new Error('The validator did not start')
    await new Promise((r) => setTimeout(r, 500))
  }
  const files = process.argv.slice(2).length ? process.argv.slice(2) : ['tests/hub_registry.test.mjs', 'tests/demo_pool.test.mjs']
  status = spawnSync(process.execPath, ['--test', '--test-concurrency=1', ...files], { cwd: chainDir, stdio: 'inherit' }).status ?? 1
} finally {
  // Stop exactly the validator this run started.
  if (validator.exitCode === null) {
    validator.kill('SIGTERM')
    await new Promise((r) => validator.once('exit', r))
  }
  if (status === 0) rmSync(state, { recursive: true, force: true })
}
process.exit(status)
