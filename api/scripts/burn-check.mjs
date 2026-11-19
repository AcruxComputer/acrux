// One /v1/burn route check with actual local Postgres persistence and faked HTTP RPC only.
// Run after migrations and npm.cmd run build: node scripts/burn-check.mjs
// It opens no server, sends no transaction, and removes only rows under its unique test keys.
import assert from 'node:assert/strict'
import { randomBytes, randomUUID } from 'node:crypto'
import { fileURLToPath } from 'node:url'
import { ACRUX_BRIDGE_IDL, Burn, CONTRACT_VERSION, toBase58 } from '@acrux/contract'
import { BorshInstructionCoder } from '@coral-xyz/anchor'
import BN from 'bn.js'
import { TOKEN_PROGRAM_ID } from '@solana/spl-token'
import { Keypair, PublicKey, SystemProgram, TransactionInstruction, TransactionMessage, VersionedTransaction } from '@solana/web3.js'
import { createBurnReader, mountBurnRoutes, readNodeFeeBurns } from '../dist/burn.js'
import { connectDatabase } from '../dist/db.js'
import { createApi } from '../dist/engines/server/app.js'
import { createRpc } from '../dist/engines/rpc/client.js'

process.loadEnvFile(fileURLToPath(new URL('../.env', import.meta.url)))
assert.ok(process.env.DATABASE_URL, 'Set DATABASE_URL in api/.env.')
const databaseUrl = new URL(process.env.DATABASE_URL)
assert.ok(['127.0.0.1', 'localhost'].includes(databaseUrl.hostname) && databaseUrl.port === '4921', 'This check requires the local Postgres on port 4921.')
databaseUrl.searchParams.set('options', '-c statement_timeout=10000')
const database = connectDatabase(databaseUrl.toString()), db = database.db
const program = Keypair.generate().publicKey, mint = Keypair.generate().publicKey, wallet = Keypair.generate(), feeWallet = Keypair.generate()
const programName = program.toBase58(), mintName = mint.toBase58(), walletName = wallet.publicKey.toBase58()
const gatewayUrl = 'http://127.0.0.1:5047', solanaUrl = 'http://127.0.0.1:5048'
const epochSlots = 1200, firstEpoch = 8, secondEpoch = 9, runningEpoch = 10
const firstSlot = epoch => epoch * epochSlots
const clockStart = Date.now() - 150_000
const at = seconds => new Date(clockStart + seconds * 1000).toISOString()
const nodeEpochs = [
  { epoch: runningEpoch, firstSlot: firstSlot(runningEpoch), lastSlot: firstSlot(runningEpoch) + epochSlots - 1, startedAt: clockStart + 120_000, endedAt: null, closed: false, baseFeeBurnedLamports: '750', feeBurnAmountBaseUnits: null, remainderLamports: null },
  { epoch: secondEpoch, firstSlot: firstSlot(secondEpoch), lastSlot: firstSlot(secondEpoch) + epochSlots - 1, startedAt: clockStart + 60_000, endedAt: clockStart + 120_000, closed: true, baseFeeBurnedLamports: '1999', feeBurnAmountBaseUnits: '2', remainderLamports: '0' },
  { epoch: firstEpoch, firstSlot: firstSlot(firstEpoch), lastSlot: firstSlot(firstEpoch) + epochSlots - 1, startedAt: clockStart, endedAt: clockStart + 60_000, closed: true, baseFeeBurnedLamports: '1001', feeBurnAmountBaseUnits: '1', remainderLamports: '1' },
]
const nodePage = { program: programName, mint: mintName, nativeFromSlot: firstSlot(firstEpoch), epochSlots, items: nodeEpochs, nextCursor: null, feeBurnedBaseUnits: '3' }
const u64 = value => { const bytes = Buffer.alloc(8); bytes.writeBigUInt64LE(BigInt(value)); return bytes }
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const coder = new BorshInstructionCoder(ACRUX_BRIDGE_IDL)
function signedBurn(epoch, amount) {
  const blockhash = Keypair.generate().publicKey.toBase58()
  const instruction = new TransactionInstruction({
    programId: program,
    keys: [
      { pubkey: feeWallet.publicKey, isSigner: true, isWritable: true },
      { pubkey: pda(Buffer.from('bridge')), isSigner: false, isWritable: false },
      { pubkey: pda(Buffer.from('fee_burn_config')), isSigner: false, isWritable: false },
      { pubkey: pda(Buffer.from('cblock'), Buffer.alloc(32, epoch)), isSigner: false, isWritable: false },
      { pubkey: mint, isSigner: false, isWritable: true },
      { pubkey: pda(Buffer.from('vault'), mint.toBuffer()), isSigner: false, isWritable: true },
      { pubkey: pda(Buffer.from('fee_burn'), u64(Math.floor(epoch / 8192))), isSigner: false, isWritable: true },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
    ],
    data: coder.encode('burn', { epoch: new BN(epoch), amount: new BN(amount), proof: [] }),
  })
  const transaction = new VersionedTransaction(new TransactionMessage({ payerKey: feeWallet.publicKey, recentBlockhash: blockhash, instructions: [instruction] }).compileToV0Message())
  transaction.sign([feeWallet])
  return { key: `bridge:${programName}:fee-burn:${epoch}`, signature: toBase58(transaction.signatures[0]), signedBase64: Buffer.from(transaction.serialize()).toString('base64'), blockhash, lastValidBlockHeight: 100_000n, contextSlot: 10n }
}
const confirmed = signedBurn(firstEpoch, 1), pending = signedBurn(secondEpoch, 2)
const cycleIds = [randomUUID(), randomUUID()]
const originalFetch = globalThis.fetch
let liveSupply = '1000000000', supplyReads = 0, gatewayReads = 0, ownRows = false
const response = (id, result) => new Response(JSON.stringify({ jsonrpc: '2.0', id, result }), { headers: { 'content-type': 'application/json' } })
globalThis.fetch = async (input, init = {}) => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url
  const request = JSON.parse(String(init.body))
  const origin = new URL(url).origin
  if (origin === gatewayUrl && request.method === 'getFeeBurns') {
    gatewayReads++
    assert.ok(!Array.isArray(request.params), 'Acrux gateway calls take object params.')
    return response(request.id, structuredClone(nodePage))
  }
  if (origin === solanaUrl && request.method === 'getTokenSupply') {
    supplyReads++
    assert.equal(request.params[0], mintName)
    assert.equal(request.params[1].commitment, 'finalized')
    return response(request.id, { context: { slot: 1234 }, value: { amount: liveSupply, decimals: 6, uiAmount: Number(liveSupply) / 1_000_000, uiAmountString: String(Number(liveSupply) / 1_000_000) } })
  }
  throw new Error(`Unexpected RPC in burn check: ${request.method}`)
}
function route() {
  const solanaRpc = createRpc({ url: solanaUrl, service: `burn-check-${randomUUID()}`, timeoutMs: 5000, log: () => {} })
  const reader = createBurnReader({ db, gateway: query => readNodeFeeBurns(gatewayUrl, query), solanaRpc, mint: mintName, wallet: walletName, program: programName })
  const app = createApi({ origins: [], contractVersion: CONTRACT_VERSION })
  mountBurnRoutes(app, reader)
  return app
}
async function readRoute(app) {
  const result = await app.request('/v1/burn?limit=10')
  assert.equal(result.status, 200, `GET /v1/burn must be registered: ${await result.clone().text()}`)
  return Burn.parse(await result.json())
}
const epoch = (answer, number) => {
  const found = answer.epochs.items.find(item => item.epoch === number)
  assert.ok(found, `The response must contain epoch ${number}.`)
  return found
}
try {
  // Every write is scoped by fresh public test addresses. Check ownership before cleanup is armed.
  assert.equal(await db.burnSupplySample.count({ where: { program: programName, mint: mintName } }), 0)
  assert.equal(await db.buybackCycle.count({ where: { mint: mintName, wallet: walletName } }), 0)
  assert.equal(await db.keeperTransaction.count({ where: { key: { in: [confirmed.key, pending.key] } } }), 0)
  ownRows = true
  await db.$transaction(async tx => {
    for (let i = 0; i < 2; i++) {
      await tx.buybackCycle.create({ data: { id: cycleIds[i], mint: mintName, wallet: walletName, state: 'done' } })
      await tx.buybackRow.create({ data: { cycleId: cycleIds[i], mint: mintName, wallet: walletName, action: 'burn', signature: toBase58(randomBytes(64)), tokenAmount: String((i + 1) * 100), createdAt: new Date(at(i * 60 + 30)) } })
    }
    await tx.keeperTransaction.create({ data: { ...confirmed, state: 'confirmed' } })
    await tx.keeperTransaction.create({ data: { ...pending, state: 'signed' } })
  })
  const first = await readRoute(route())
  assert.equal(first.mint, mintName)
  assert.equal(first.mintSupply.amount, '1000000000')
  assert.equal(first.mintSupply.decimals, 6)
  assert.deepEqual(first.totals, { baseFeeBurnBaseUnits: '1', buybackBurnBaseUnits: '300' }, 'Pending FeeBurns must not count as burns already executed on Solana.')
  const firstClosed = epoch(first, firstEpoch), secondClosed = epoch(first, secondEpoch), running = epoch(first, runningEpoch)
  assert.equal(firstClosed.baseFeeBurnedLamports, '1001')
  assert.equal(firstClosed.feeBurnAmountBaseUnits, '1')
  assert.equal(firstClosed.remainderLamports, '1')
  assert.deepEqual(firstClosed.solanaBurn, { signature: confirmed.signature, state: 'burned' })
  assert.equal(firstClosed.buybackBurnBaseUnits, '100')
  assert.deepEqual(secondClosed.solanaBurn, { signature: pending.signature, state: 'pending' })
  assert.equal(secondClosed.buybackBurnBaseUnits, '200')
  for (const closed of [firstClosed, secondClosed]) {
    assert.ok(closed.closed)
    assert.equal(closed.mintSupplyAtEnd.amount, '1000000000')
    assert.equal(closed.mintSupplyAtEnd.decimals, 6)
  }
  assert.equal(running.closed, false)
  assert.equal(running.baseFeeBurnedLamports, '750')
  assert.equal(running.feeBurnAmountBaseUnits, null)
  assert.deepEqual(running.solanaBurn, { signature: null, state: 'not_yet_due' })
  assert.equal(running.buybackBurnBaseUnits, '0')
  assert.equal(running.mintSupplyAtEnd, null)
  const saved = await db.burnSupplySample.findMany({ where: { program: programName, mint: mintName }, orderBy: { epoch: 'asc' } })
  assert.deepEqual(saved.map(row => [Number(row.epoch), row.amount.toFixed(0), row.decimals]), [[firstEpoch, '1000000000', 6], [secondEpoch, '1000000000', 6]])
  assert.equal(supplyReads, 1, 'The page shares one live Solana mint-supply read.')

  // A new reader models an API restart: live RPC supply moves, persisted closed samples stay put.
  liveSupply = '999999000'
  const second = await readRoute(route())
  assert.equal(second.mintSupply.amount, '999999000')
  assert.notDeepEqual(second.mintSupply, first.mintSupply)
  assert.deepEqual(epoch(second, firstEpoch).mintSupplyAtEnd, firstClosed.mintSupplyAtEnd)
  assert.deepEqual(epoch(second, secondEpoch).mintSupplyAtEnd, secondClosed.mintSupplyAtEnd)
  assert.equal(epoch(second, runningEpoch).mintSupplyAtEnd, null)
  assert.deepEqual(second.totals, first.totals)
  const savedAfter = await db.burnSupplySample.findMany({ where: { program: programName, mint: mintName }, orderBy: { epoch: 'asc' } })
  assert.deepEqual(savedAfter.map(row => [row.epoch.toString(), row.amount.toFixed(0), row.sampledAt.toISOString()]), saved.map(row => [row.epoch.toString(), row.amount.toFixed(0), row.sampledAt.toISOString()]))
  assert.equal(supplyReads, 2)
  assert.equal(gatewayReads, 2)
  console.log(JSON.stringify({ event: 'burn_check_passed', closedEpochs: [firstEpoch, secondEpoch], runningEpoch, confirmedFeeBurnBaseUnits: first.totals.baseFeeBurnBaseUnits, buybackBurnBaseUnits: first.totals.buybackBurnBaseUnits, originalLiveSupply: first.mintSupply.amount, nextLiveSupply: second.mintSupply.amount, immutableClosedSamples: savedAfter.length }))
} finally {
  globalThis.fetch = originalFetch
  try {
    if (ownRows) await db.$transaction([
      db.buybackRow.deleteMany({ where: { mint: mintName, wallet: walletName } }),
      db.buybackCycle.deleteMany({ where: { mint: mintName, wallet: walletName } }),
      db.keeperTransaction.deleteMany({ where: { key: { in: [confirmed.key, pending.key] } } }),
      db.burnSupplySample.deleteMany({ where: { program: programName, mint: mintName } }),
    ])
  } finally { await database.close() }
}
