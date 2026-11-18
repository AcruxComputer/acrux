// Attach to an existing short-epoch bridge stand and its running keeper. Start no processes and
// send no transactions. The next epoch's validator set must be confirmed within 30 seconds of
// observing Acrux's finalized frontier cross that epoch boundary.
// Usage: node scripts/bridge-handoff-check.mjs <stand dir> [gateway URL] [local Solana RPC URL]
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { Connection, PublicKey } from '@solana/web3.js'
import { chainDir } from './dev-keys.mjs'
import { chainTag, pdas } from './bridge-relay.mjs'

const [standDir, gateway = 'http://127.0.0.1:4930', solanaUrl = 'http://127.0.0.1:4990'] = process.argv.slice(2)
if (!standDir) throw new Error('Give the existing bridge stand directory.')
for (const value of [gateway, solanaUrl]) {
  const url = new URL(value)
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) throw new Error('This check accepts local HTTP RPCs only.')
}
const stand = JSON.parse(readFileSync(join(standDir, 'stand.json'), 'utf8'))
const program = new PublicKey(stand.program), addresses = pdas(program)
const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8'))
const coder = new anchor.BorshAccountsCoder(idl)
const connection = new Connection(solanaUrl, { commitment: 'confirmed', disableRetryOnRateLimit: true,
  fetch: (input, init) => fetch(input, { ...init, signal: AbortSignal.timeout(5_000) }) })
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const proof = async () => {
  const response = await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getCblockProof', params: { latest: true } }), signal: AbortSignal.timeout(5_000) })
  const body = await response.json()
  if (!response.ok || body.error || !body.result) throw new Error('The gateway has no finalized bridge proof.')
  return body.result
}
const bridgeInfo = await connection.getAccountInfo(addresses.bridge, 'confirmed')
if (!bridgeInfo || !bridgeInfo.owner.equals(program)) throw new Error('The stand bridge is not initialized.')
const bridge = coder.decode('Bridge', bridgeInfo.data)
const epochSlots = bridge.epoch_slots.toNumber()
const start = await proof()
if (start.epochSlots !== epochSlots || !chainTag(start.chainId).equals(Buffer.from(bridge.chain_id))) throw new Error('The gateway and Solana bridge rules differ.')
const epoch = Math.floor((start.consensusSlot * 4 + 3) / epochSlots) + 1
const boundarySlot = epoch * epochSlots
const boundaryDeadline = Date.now() + 180_000
let crossed
while (Date.now() < boundaryDeadline) {
  const latest = await proof()
  if (latest.consensusSlot * 4 + 3 >= boundarySlot) { crossed = latest; break }
  await pause(250)
}
if (!crossed) throw new Error('The next finalized epoch boundary did not arrive within 180 seconds; use a short-epoch stand.')
const observedAt = Date.now(), deadline = observedAt + 30_000
while (Date.now() < deadline) {
  const info = await connection.getAccountInfo(addresses.set(epoch), 'confirmed')
  const seenAt = Date.now()
  if (seenAt > deadline) break
  if (info) {
    if (!info.owner.equals(program) || info.executable) throw new Error('Validator set account has an invalid owner.')
    const set = coder.decode('ValidatorSet', info.data)
    if (set.epoch.toNumber() !== epoch || !set.validators.length) throw new Error('The confirmed validator set is invalid.')
    console.log(JSON.stringify({ event: 'bridge_handoff_check', epoch, boundaryCblock: crossed.consensusSlot,
      confirmedWithinMs: seenAt - observedAt, validators: set.validators.length }))
    process.exit(0)
  }
  await pause(250)
}
throw new Error(`Epoch ${epoch} was not handed on at confirmed commitment within 30 seconds of the finalized boundary.`)
