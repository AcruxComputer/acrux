// A JSON-RPC call that leaves `params` out, as web3.js and wallets send getSlot and
// getLatestBlockhash, answers in Solana's form; Acrux-only methods still answer without params.
// A local net (gateway 5041, events 5042, private port 5043, QUIC 5062..5073, a temporary store
// under node/target). Usage from backend/chain: node scripts/rpc-omitted-params-check.mjs
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { join, resolve, sep } from 'node:path'
import { chainDir } from './dev-keys.mjs'

const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const gateway = 'http://127.0.0.1:5041'
const pause = ms => new Promise(r => setTimeout(r, ms))
let runDir, child, tail = ''

// The raw body, with no params field at all.
const call = async method => (await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method }) })).json()

try {
  runDir = mkdtempSync(join(target, 'rpc-omitted-params-'))
  const env = { ...process.env, ACRUX_BIND: '127.0.0.1' }
  for (const name of ['ACRUX_BRIDGE', 'ACRUX_PLACEMENTS', 'ACRUX_LOG_RPC']) delete env[name]
  child = spawn(binary, ['net', 'up', '--genesis', join(chainDir, 'genesis/acrux-net.json'), '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062', '--traffic-rate', '0'], { cwd: nodeDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-64_000) })
  for (let i = 0; i < 180; i++) {
    if (child.exitCode !== null) throw new Error(`The local net exited: ${tail.slice(-4000)}`)
    const head = await call('getFinalizedCblock').catch(() => null)
    if (head?.result?.consensusSlot != null) break
    await pause(500)
  }
  const slot = await call('getSlot')
  assert.ok(Number.isSafeInteger(slot.result), `getSlot with no params answers Solana's slot number: ${JSON.stringify(slot)}`)
  const blockhash = await call('getLatestBlockhash')
  assert.ok(typeof blockhash.result?.value?.blockhash === 'string' && Number.isSafeInteger(blockhash.result?.value?.lastValidBlockHeight), `getLatestBlockhash with no params answers Solana's shape: ${JSON.stringify(blockhash)}`)
  const cblock = await call('getFinalizedCblock')
  assert.ok(Number.isSafeInteger(cblock.result?.consensusSlot), 'An Acrux-only method still answers without params.')
  const balance = await call('getBalance')
  const empty = await (await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getBalance', params: [] }) })).json()
  assert.deepEqual(balance, empty, 'getBalance with params left out answers exactly as with params: [].')
  console.log(JSON.stringify({ event: 'rpc_omitted_params_check_passed', getSlot: slot.result, getLatestBlockhash: blockhash.result.value.blockhash, getFinalizedCblock: cblock.result.consensusSlot, getBalance: balance.error.message }))
} finally {
  if (child && child.exitCode === null) {
    const exited = new Promise(r => child.once('exit', r))
    child.kill(); await Promise.race([exited, pause(5000)])
  }
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep))
    rmSync(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
