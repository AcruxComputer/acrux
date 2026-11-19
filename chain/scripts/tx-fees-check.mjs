// Build 4 step 2: what a transaction paid, on its record. A local net (gateway 5041, events 5042,
// private port 5043, QUIC 5062..5073, a temporary store under node/target), a fresh key funded by
// the faucet, one stock @solana/web3.js transfer. The Acrux-shape getTransaction carries `fees`
// (base, priority, inclusion, burned, total, in lamports: native ACRUX from the cut-over), its total
// is Solana's meta.fee and the payer's balance fell by exactly the transfer and that total.
// Usage from backend/chain: node scripts/tx-fees-check.mjs
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import dgram from 'node:dgram'
import { mkdtempSync, rmSync } from 'node:fs'
import net from 'node:net'
import { join, resolve, sep } from 'node:path'
import { ComputeBudgetProgram, Connection, Keypair, SystemProgram, Transaction } from '@solana/web3.js'
import { chainDir } from './dev-keys.mjs'

const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const gateway = 'http://127.0.0.1:5041', internal = 'http://127.0.0.1:5043'
const acrux = new Connection(gateway, 'confirmed')
const payer = Keypair.generate(), receiver = Keypair.generate().publicKey
const amount = 1_000_000, granted = 2_000_000_000, microLamportsPerCu = 1_000_000, computeLimit = 10_000
const pause = ms => new Promise(r => setTimeout(r, ms))
let runDir, child, tail = ''

async function until(label, seconds, read) {
  for (const deadline = Date.now() + seconds * 1000; Date.now() < deadline; await pause(500)) {
    if (child.exitCode !== null) throw new Error(`The local net exited: ${tail.slice(-4000)}`)
    const value = await read().catch(() => null)
    if (value) return value
  }
  throw new Error(`${label}: not seen within ${seconds} s`)
}
async function rpc(method, params) {
  const body = await (await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()
  if (body.error) throw new Error(`${method}: ${JSON.stringify(body.error)}`)
  return body.result
}
async function freePorts() {
  for (const port of [5041, 5042, 5043]) {
    const server = net.createServer()
    await new Promise((ok, fail) => { server.once('error', fail); server.listen({ port, host: '127.0.0.1', exclusive: true }, ok) })
    await new Promise(ok => server.close(ok))
  }
  for (let port = 5062; port <= 5073; port++) {
    const socket = dgram.createSocket('udp4')
    await new Promise((ok, fail) => { socket.once('error', fail); socket.bind({ port, address: '0.0.0.0', exclusive: true }, ok) })
    socket.close()
  }
}

try {
  await freePorts()
  runDir = mkdtempSync(join(target, 'tx-fees-check-'))
  const env = { ...process.env, ACRUX_BIND: '127.0.0.1' }
  for (const name of ['ACRUX_BRIDGE', 'ACRUX_PLACEMENTS', 'ACRUX_LOG_RPC']) delete env[name]
  child = spawn(binary, ['net', 'up', '--genesis', join(chainDir, 'genesis/acrux-net.json'), '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062', '--traffic-rate', '0'], { cwd: nodeDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-64_000) })
  await until('The local net', 90, async () => (await rpc('getFinalizedCblock', {})).consensusSlot !== null)

  const grant = await fetch(`${internal}/internal/faucet`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address: payer.publicKey.toBase58(), amountLamports: String(granted) }) })
  assert.ok(grant.ok, `The faucet grant: HTTP ${grant.status}`)
  const before = await until('The grant', 60, async () => (await acrux.getBalance(payer.publicKey, 'finalized')) || null)

  const latest = await acrux.getLatestBlockhash('confirmed')
  const transfer = new Transaction({ feePayer: payer.publicKey, ...latest }).add(
    ComputeBudgetProgram.setComputeUnitLimit({ units: computeLimit }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: microLamportsPerCu }),
    SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: receiver, lamports: amount }),
  )
  transfer.sign(payer)
  const signature = await acrux.sendRawTransaction(transfer.serialize(), { preflightCommitment: 'confirmed' })
  await until('Finality', 90, async () => (await acrux.getSignatureStatuses([signature])).value[0]?.confirmationStatus === 'finalized')

  const record = await until('The Acrux record', 30, () => rpc('getTransaction', { signature }))
  const solana = await until('The Solana record', 30, () => acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 }))
  const after = await acrux.getBalance(payer.publicKey, 'finalized')
  assert.ok(record.fees, 'The Acrux transaction record carries what its payer paid (fees).')
  const fees = Object.fromEntries(Object.entries(record.fees).map(([k, v]) => [k, BigInt(v)]))
  assert.equal(fees.total, fees.base + fees.priority + fees.inclusion)
  assert.equal(fees.burned, fees.base / 2n, 'Half the base fee is burned.')
  assert.equal(fees.priority, BigInt(Math.ceil(microLamportsPerCu * computeLimit / 1_000_000)))
  assert.ok(fees.base > 0n && fees.inclusion > 0n)
  assert.equal(fees.total, BigInt(solana.meta.fee), 'The record total is Solana meta.fee.')
  assert.equal(BigInt(before - after), BigInt(amount) + fees.total, 'The payer paid exactly the transfer and the fees.')
  console.log(JSON.stringify({ event: 'tx_fees_check_passed', signature, fees: record.fees, metaFee: solana.meta.fee, payerDelta: before - after, amount }))
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
