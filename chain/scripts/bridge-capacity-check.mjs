// Run against an operator-started local Solana bridge stand. Start one separate Acrux net with
// archive shedding level one, deposit test tokens, burn twice, and prove slots/proofs
// continue above the level. No keeper is started and neither withdrawal is released on Solana.
// Own ports from ACRUX_CHECK_BASE_PORT (5041 by default): QUIC base..base+11, gateway base+12/13, private metrics base+14. Stop only the spawned net
// and remove its unique node/target directory. The operator's validator and stand stay running.
// Usage: node scripts/bridge-capacity-check.mjs <stand dir> [local Solana RPC URL] [acrux binary]
import { spawn, spawnSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { createSocket } from 'node:dgram'
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:net'
import { basename, dirname, join, resolve } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { bridgeProgram, decodeEntry, pdas } from './bridge-relay.mjs'

const [standDir, solanaUrl = 'http://127.0.0.1:4990', binary] = process.argv.slice(2)
if (!standDir) throw new Error('Give an existing bridge stand directory containing stand.json and user-keypair.json.')
const url = new URL(solanaUrl)
if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) throw new Error('This check accepts a local HTTP Solana RPC only.')
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running this check.')
const chain = resolve(import.meta.dirname, '..'), node = resolve(chain, '../node')
const target = realpathSync(join(node, 'target'))
const executable = binary ? resolve(binary) : join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const stand = JSON.parse(readFileSync(join(standDir, 'stand.json'), 'utf8'))
const user = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(join(standDir, 'user-keypair.json'), 'utf8'))))
if (user.publicKey.toBase58() !== stand.user) throw new Error('The stand user keypair does not match stand.json.')
const programId = new PublicKey(stand.program), mint = new PublicKey(stand.mint)
const base = Number(process.env.ACRUX_CHECK_BASE_PORT ?? 5041)
const gateway = `http://127.0.0.1:${base + 12}`, metrics = `http://127.0.0.1:${base + 14}`
const chainId = `acrux-bridge-capacity-${randomUUID()}`
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true,
  fetch: (input, init) => fetch(input, { ...init, signal: AbortSignal.timeout(10_000) }) })
const solana = connection(solanaUrl), acrux = connection(gateway)
const solanaBridge = bridgeProgram(solana, user), onAcrux = bridgeProgram(acrux, user)
if (!solanaBridge.programId.equals(programId)) throw new Error('The built bridge IDL does not match the stand program.')
const u64 = value => { const bytes = Buffer.alloc(8); bytes.writeBigUInt64LE(BigInt(value.toString())); return bytes }
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, programId)[0]
const rpc = async (method, params) => {
  const response = await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(10_000) })
  const body = await response.json()
  if (!response.ok || body.error) throw new Error(`${method} failed on the capacity-check net.`)
  return body.result
}
let net, netError, fullWarnings = 0
const until = async (label, seconds, read) => {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    if (netError || (net && (net.exitCode !== null || net.signalCode !== null))) throw new Error('The capacity-check net stopped unexpectedly.')
    const value = await read()
    if (value) return value
    await pause(250)
  }
  throw new Error(`${label} did not complete within ${seconds} seconds.`)
}
const send = async (rpc, instruction) => {
  const latest = await rpc.getLatestBlockhash('confirmed')
  const tx = new Transaction({ feePayer: user.publicKey, ...latest }).add(instruction)
  tx.sign(user)
  const signature = await rpc.sendRawTransaction(tx.serialize(), { preflightCommitment: 'confirmed', maxRetries: 0 })
  await until('transaction confirmation', 30, async () => {
    const status = (await rpc.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
    if (status?.err) throw new Error('A capacity-check transaction failed on chain.')
    return status?.confirmationStatus === 'confirmed' || status?.confirmationStatus === 'finalized'
  })
}
// Refuse occupied ports rather than accidentally talking to another session's net.
for (const port of [base + 12, base + 13, base + 14]) await new Promise((resolve, reject) => {
  const server = createServer(); server.once('error', reject); server.listen(port, '127.0.0.1', () => server.close(resolve))
})
for (let port = base; port <= base + 11; port++) await new Promise((resolve, reject) => {
  const socket = createSocket('udp4'); socket.once('error', error => { socket.close(); reject(error) }); socket.bind(port, '127.0.0.1', () => socket.close(resolve))
})
const store = mkdtempSync(join(target, 'bridge-capacity-'))
try {
  mkdirSync(join(store, 'placements')); mkdirSync(join(store, 'genesis'))
  const rulePath = join(store, 'bridge.json'), genesisPath = join(store, 'genesis', 'acrux-net.json')
  writeFileSync(rulePath, JSON.stringify({ program: stand.program, fromSlot: 0, epochSlots: 1536, chainId }))
  const env = { ...process.env, RUST_LOG: 'warn', ACRUX_BIND: '127.0.0.1', ACRUX_BRIDGE_PROOF_MAX: '1', ACRUX_BRIDGE: rulePath,
    ACRUX_BRIDGE_RPC: `${solanaUrl},${solanaUrl}`, ACRUX_PLACEMENTS: join(store, 'placements', 'placements.json') }
  for (const args of [['scripts/place-bridge.mjs', join(store, 'placements', 'bridge.json')], ['scripts/genesis.mjs', genesisPath]]) {
    const result = spawnSync(process.execPath, args, { cwd: chain, env, stdio: 'pipe', timeout: 60_000, windowsHide: true })
    if (result.error || result.status !== 0) throw new Error('Capacity-check placement/genesis preparation failed.')
  }
  writeFileSync(env.ACRUX_PLACEMENTS, JSON.stringify({ placements: [{ file: 'bridge.json', slot: 0 }] }))
  const genesis = JSON.parse(readFileSync(genesisPath, 'utf8'))
  for (const entry of genesis.programs) entry.path = resolve(chain, entry.path)
  writeFileSync(genesisPath, JSON.stringify(genesis))
  net = spawn(executable, ['net', 'up', '--genesis', genesisPath, '--store', join(store, 'net'), '--base-port', String(base),
    '--gateway-port', String(base + 12), '--ws-port', String(base + 13), '--metrics-port', String(base + 14), '--traffic-rate', '0'], { cwd: node, env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  net.once('error', error => { netError = error })
  const consume = stream => {
    let pending = ''
    stream.on('data', chunk => {
      pending += chunk.toString()
      const lines = pending.split('\n'); pending = lines.pop().slice(-16_384)
      fullWarnings += lines.filter(line => line.includes('bridge_proof_archive_full')).length
    })
  }
  consume(net.stdout); consume(net.stderr)
  const opening = await until('initial finalized proof', 60, async () => {
    try { const value = await rpc('getCblockProof', { latest: true }); return value?.chainId === chainId ? value : null } catch { return null }
  })
  const granted = await fetch(`${metrics}/internal/faucet`, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ address: stand.user, amountLamports: '2000000000' }), signal: AbortSignal.timeout(10_000) })
  if (!granted.ok) throw new Error('The capacity-check faucet grant failed.')
  const { bridge, allowed } = pdas(programId)
  const source = getAssociatedTokenAddressSync(mint, user.publicKey)
  const sequence = (await solanaBridge.account.bridge.fetch(bridge)).deposits
  await send(solana, await solanaBridge.methods.deposit(new anchor.BN(3_000_000), user.publicKey).accountsStrict({
    depositor: user.publicKey, bridge, mint, source, record: pda(Buffer.from('deposit'), u64(sequence)), allowed: allowed(mint),
    vault: pda(Buffer.from('vault'), mint.toBuffer()), tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    eventAuthority: pda(Buffer.from('__event_authority')), program: programId,
  }).instruction())
  const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
  const bridgedAccount = getAssociatedTokenAddressSync(bridgedMint, user.publicKey)
  await until('bridged credit', 90, async () => {
    const info = await acrux.getAccountInfo(bridgedAccount, 'confirmed')
    return info && info.data.length >= 72 && info.data.readBigUInt64LE(64) >= 3_000_000n
  })
  const withdrawalCblocks = []
  for (let index = 0; index < 2; index++) {
    if (withdrawalCblocks.length) await until('a later finalized cblock', 15, async () => (await rpc('getCblockProof', { latest: true })).consensusSlot > withdrawalCblocks[0])
    const outbound = pda(Buffer.from('outbound'))
    const previous = await onAcrux.account.outbound.fetchNullable(outbound)
    const next = previous?.withdrawals ?? new anchor.BN(0)
    const record = pda(Buffer.from('withdrawal'), u64(next))
    await send(acrux, await onAcrux.methods.withdraw(mint, new anchor.BN(1_000_000), user.publicKey).accountsStrict({
      owner: user.publicKey, mint: bridgedMint, minimum: pda(Buffer.from('minimum'), mint.toBuffer()),
      source: bridgedAccount, outbound, record, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }).instruction())
    const withdrawal = await onAcrux.account.withdrawal.fetch(record)
    const cblock = Math.floor(withdrawal.slot.toNumber() / 4)
    const held = await until('later withdrawal proof', 30, async () => {
      try { return await rpc('getCblockProof', { consensusSlot: cblock }) } catch { return null }
    })
    if (!held.outbox.some(leaf => { const entry = decodeEntry(Buffer.from(leaf, 'base64')); return entry.withdrawal && entry.sequence.eq(next) })) throw new Error('The finalized proof omitted its withdrawal.')
    withdrawalCblocks.push(cblock)
  }
  if (withdrawalCblocks[1] <= withdrawalCblocks[0]) throw new Error('The second withdrawal did not land in a later cblock.')
  const after = await until('continued finalization after both withdrawals', 15, async () => {
    const latest = await rpc('getCblockProof', { latest: true }); return latest.consensusSlot > withdrawalCblocks[1] ? latest : null
  })
  await until('archive above-level warning', 5, async () => fullWarnings > 0)
  if (fullWarnings !== 1) throw new Error('An archive above its shedding level must emit exactly one transition warning.')
  console.log(JSON.stringify({ event: 'bridge_capacity_check', capacity: 1, fullWarnings, firstCblock: opening.consensusSlot,
    withdrawalCblocks, laterCblock: after.consensusSlot, proofsServed: true }))
} finally {
  if (net && net.exitCode === null && net.signalCode === null && !netError) {
    await new Promise((resolve, reject) => {
      let forced
      const timer = setTimeout(() => {
        net.kill('SIGKILL')
        forced = setTimeout(() => reject(new Error('The capacity-check net did not stop after termination.')), 5_000)
      }, 10_000)
      net.once('exit', () => { clearTimeout(timer); clearTimeout(forced); resolve() }); net.kill()
    })
  }
  if (dirname(realpathSync(store)) !== target || !basename(store).startsWith('bridge-capacity-')) throw new Error('Refusing cleanup outside the capacity-check directory.')
  rmSync(store, { recursive: true, force: true })
}
