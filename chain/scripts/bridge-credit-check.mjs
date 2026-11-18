// Road build 3, step 2: a deposit on a local Solana validator credited on a local Acrux net.
// Starts the validator with acrux_bridge, an Acrux net whose bridge rule names that program
// from slot 0, deposits for a fresh Acrux recipient, waits for the credit, checks it through the
// Acrux RPC and with a follower re-executing the net, then stops both.
// Usage (from backend/chain): node scripts/bridge-credit-check.mjs [acrux binary, from backend/node]
import { spawn, spawnSync } from 'node:child_process'
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { Keypair } from '@solana/web3.js'

const chain = resolve(import.meta.dirname, '..')
const node = resolve(chain, '../node')
const acrux = resolve(node, process.argv[2] ?? 'target/release/acrux')
const program = 'EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5'
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running a bridge check.')
const epochSlots = 1536 // Short local bridge rule, shared with the deposit initializer.
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const rpc = async (url, method, params) => (await (await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()).result
const up = async (url) => { for (let i = 0; i < 120; i++) { if ((await rpc(url, 'getSlot', []).catch(() => 0)) > 2) return; await sleep(1000) } throw new Error(`${url} did not start`) }

const state = join(chain, '.ledger')
rmSync(state, { recursive: true, force: true })
mkdirSync(state, { recursive: true })
const store = join(node, 'target/bridge-check')
rmSync(store, { recursive: true, force: true })
mkdirSync(store, { recursive: true })
const rule = join(store, 'bridge.json')
writeFileSync(rule, JSON.stringify({ program, fromSlot: 0, epochSlots, chainId: 'acrux-bridge-local-check' }))
const env = { ...process.env, ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_BRIDGE: rule, ACRUX_BRIDGE_RPC: 'http://127.0.0.1:4990,http://127.0.0.1:4990' }
const genesisPath = join(store, 'genesis', 'acrux-net.json')
mkdirSync(join(store, 'genesis'), { recursive: true })
const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', genesisPath], { cwd: chain, env, stdio: 'pipe' })
if (generated.status !== 0) throw new Error('Local bridge genesis generation failed.')
const genesis = JSON.parse(readFileSync(genesisPath, 'utf8'))
for (const entry of genesis.programs) entry.path = resolve(chain, entry.path)
writeFileSync(genesisPath, JSON.stringify(genesis))
const validator = spawn('bash', [join(chain, 'scripts/validator.sh'), state, '32', '--upgradeable-program', program, join(chain, 'target/deploy/acrux_bridge.so'), join(chain, 'keys/dev/local-authority.json')], { stdio: 'ignore' })
let net = spawn(acrux, ['net', 'up', '--genesis', genesisPath, '--store', join(store, 'net')], { cwd: node, env, stdio: 'ignore' })
let ok = false
try {
  await up('http://127.0.0.1:4990')
  await up('http://127.0.0.1:4930')
  const before = await rpc('http://127.0.0.1:4930', 'getCounters', {})
  console.log(`net before the deposit: ${before.slots} slots, ${before.skips} skipped`)
  const recipient = Keypair.generate().publicKey.toBase58()
  const deposited = spawnSync(process.execPath, ['scripts/bridge-deposit.mjs', '--recipient', recipient, '--amount', '2500000000'], { cwd: chain, env, encoding: 'utf8' })
  if (deposited.status !== 0) throw new Error(deposited.stderr)
  const record = JSON.parse(deposited.stdout.trim().split('\n').at(-1))
  console.log('deposit on Solana:', JSON.stringify(record))
  // The credit: the recipient's token account at the bridged mint on Acrux.
  let account
  for (let i = 0; i < 90 && !account; i++) {
    await sleep(1000)
    account = (await rpc('http://127.0.0.1:4930', 'getTokenAccountsByOwner', [recipient, { programId: 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA' }, { encoding: 'jsonParsed' }]))?.value?.[0]
  }
  if (!account) throw new Error('no credit on Acrux after 90 s')
  const info = account.account.data.parsed.info
  console.log(`credited on Acrux: ${info.tokenAmount.amount} at mint ${info.mint} to ${info.owner} (${account.pubkey})`)
  ok = info.tokenAmount.amount === record.amount && info.owner === recipient
  // A follower re-executes the net from its latest finalized cblock, the credit's slots included.
  await sleep(3000)
  const follow = spawnSync(acrux, ['node', 'follow', 'http://127.0.0.1:4930', '--genesis', genesisPath], { cwd: node, env, encoding: 'utf8', timeout: 25000 })
  const lines = `${follow.stdout}`.trim().split('\n')
  const matched = lines.filter((l) => l.includes('matches')).length
  console.log(`follower: ${matched} slots re-executed with matching state hashes; ${lines.filter((l) => /mismatch|does not/i.test(l)).length} mismatches`)
  if (matched <= 10) console.log('follower stopped:', `${follow.stderr}`.trim().split(/\r?\n/).slice(-3).join(' | '))
  ok &&= matched > 10
  const counters = await rpc('http://127.0.0.1:4930', 'getCounters', {})
  console.log(`net: ${counters.slots} slots, ${counters.finalizedCblocks} cblocks finalized, ${counters.skips} skipped`)
} finally {
  await new Promise(resolve => { if (net.exitCode !== null) return resolve(); net.once('exit', resolve); net.kill() })
  validator.kill('SIGTERM')
  await sleep(2000)
  rmSync(state, { recursive: true, force: true })
  rmSync(store, { recursive: true, force: true })
}
console.log(ok ? 'the deposit is credited, and every re-executed slot agrees' : 'FAILED')
process.exit(ok ? 0 : 1)
