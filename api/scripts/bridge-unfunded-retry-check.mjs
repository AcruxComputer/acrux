// A relayer without SOL: every send is refused, so each new withdrawal is kept unpaid and retried
// a minute later, not after the 2, 5 and 60 minute backoff; a funded relayer whose payout fails
// for another reason keeps that backoff. Only the HTTP RPC boundary and Prisma are in memory.
// Run after npm.cmd run build: node scripts/bridge-unfunded-retry-check.mjs
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { ACRUX_BRIDGE_IDL } from '@acrux/contract'
import { BorshAccountsCoder } from '@coral-xyz/anchor'
import BN from 'bn.js'
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { Keypair, PublicKey } from '@solana/web3.js'
import { createBridgeRelayer } from '../dist/bridge-relayer/service.js'

const program = new PublicKey(ACRUX_BRIDGE_IDL.address)
const signer = Keypair.fromSeed(Buffer.alloc(32, 7))
const other = Keypair.fromSeed(Buffer.alloc(32, 8)).publicKey
const mint = Keypair.fromSeed(Buffer.alloc(32, 9)).publicKey
const recipient = Keypair.fromSeed(Buffer.alloc(32, 10)).publicKey
const epochSlots = 400, count = 2, chainId = 'acrux-unfunded-regression'
const sha256 = (...parts) => createHash('sha256').update(Buffer.concat(parts)).digest()
const u64 = value => { const bytes = Buffer.alloc(8); bytes.writeBigUInt64LE(BigInt(value)); return bytes }
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const coder = new BorshAccountsCoder(ACRUX_BRIDGE_IDL)
const account = (data, owner = program) => ({ executable: false, owner: owner.toBase58(), lamports: 10_000_000, rentEpoch: 0, data: [Buffer.from(data).toString('base64'), 'base64'] })
const encoded = async (name, value) => account(await coder.encode(name, value))
const entries = [Buffer.concat([Buffer.alloc(4), u64(1), u64(1), other.toBuffer(), u64(1)]), ...Array.from({ length: count }, (_, sequence) => Buffer.concat([Buffer.from([1, 0, 0, 0]), u64(sequence), mint.toBuffer(), recipient.toBuffer(), u64(1)]))]
let level = entries.map(entry => sha256(entry))
while (level.length > 1) level = Array.from({ length: Math.ceil(level.length / 2) }, (_, i) => i * 2 + 1 < level.length ? sha256(level[i * 2], level[i * 2 + 1]) : level[i * 2])
const preimage = Buffer.concat([u64(50), Buffer.from('unfunded-cblock')])
const proof = { consensusSlot: 50, chainId, epochSlots, preimage: preimage.toString('base64'), outboxRoot: level[0].toString('hex'), hash: sha256(preimage, level[0]).toString('hex'), outbox: entries.map(entry => entry.toString('base64')), validators: [other.toBase58()], stakes: ['1'], votes: [] }

async function cycle(lamports) {
  const accounts = new Map(), acruxAccounts = new Map(), unpaid = new Map(), saved = new Map()
  accounts.set(pda(Buffer.from('bridge')).toBase58(), await encoded('Bridge', { deposits: new BN(0), bump: 0, authority: other, epoch_slots: new BN(epochSlots), first_epoch: new BN(0), seeded: true, chain_id: [...sha256(Buffer.from(chainId))] }))
  accounts.set(pda(Buffer.from('set'), u64(0)).toBase58(), await encoded('ValidatorSet', { epoch: new BN(0), total: new BN(1), validators: [{ key: other, stake: new BN(1) }], payer: other }))
  const mintData = Buffer.alloc(82)
  mintData.writeUInt32LE(1, 0); other.toBuffer().copy(mintData, 4); mintData.writeBigUInt64LE(1_000_000n, 36); mintData[44] = 6; mintData[45] = 1
  accounts.set(mint.toBase58(), account(mintData, TOKEN_PROGRAM_ID))
  const destinationData = Buffer.alloc(165)
  mint.toBuffer().copy(destinationData, 0); recipient.toBuffer().copy(destinationData, 32); destinationData[108] = 1
  accounts.set(getAssociatedTokenAddressSync(mint, recipient, true, TOKEN_PROGRAM_ID).toBase58(), account(destinationData, TOKEN_PROGRAM_ID))
  acruxAccounts.set(pda(Buffer.from('outbound')).toBase58(), await encoded('Outbound', { withdrawals: new BN(count), bump: 0 }))
  for (let sequence = 0; sequence < count; sequence++) acruxAccounts.set(pda(Buffer.from('withdrawal'), u64(sequence)).toBase58(), await encoded('Withdrawal', {
    sequence: new BN(sequence), owner: signer.publicKey, recipient, solana_mint: mint, amount: new BN(1), slot: new BN(proof.consensusSlot * 4), bump: 0,
  }))
  const state = { program: program.toBase58(), wallet: signer.publicKey.toBase58(), lamports: String(lamports), nextWithdrawalSequence: 0n, savedWithdrawalSequence: 0n, lastCblock: null, lastHandedEpoch: 0n, lastRelayedAt: null }
  const sequenceOf = where => Number(where.program_sequence.sequence)
  const due = (row, where) => row.nextAttemptAt <= where.nextAttemptAt.lte && (where.reason === undefined || row.reason === where.reason) && (!where.OR || where.OR.some(c => c.reason === null ? row.reason === null : row.reason !== c.reason.not))
  const db = {
    bridgeRelayerState: { upsert: async ({ update }) => Object.assign(state, update), findUniqueOrThrow: async () => state, findUnique: async () => state, update: async ({ data }) => Object.assign(state, data) },
    bridgeSavedCblockProof: {
      findUnique: async ({ where }) => saved.get(Number(where.program_cblock.cblock)) ?? null,
      upsert: async ({ where, create, update }) => { const key = Number(where.program_cblock.cblock); saved.set(key, { ...saved.get(key) ?? create, ...(saved.has(key) ? update : {}) }); return saved.get(key) },
    },
    bridgeUnpaidWithdrawal: {
      findUnique: async ({ where }) => unpaid.get(sequenceOf(where)) ?? null,
      findMany: async ({ where, select }) => [...unpaid.values()].filter(row => !where.nextAttemptAt || due(row, where)).map(row => select ? Object.fromEntries(Object.keys(select).map(key => [key, row[key]])) : row),
      upsert: async ({ where, create, update }) => { const key = sequenceOf(where); unpaid.set(key, unpaid.has(key) ? Object.assign(unpaid.get(key), update) : { ...create }); return unpaid.get(key) },
      update: async ({ where, data }) => Object.assign(unpaid.get(sequenceOf(where)), data),
      deleteMany: async () => ({ count: 0 }),
    },
    keeperTransaction: { findUnique: async () => null, findMany: async () => [], create: async ({ data }) => data, updateMany: async () => ({ count: 0 }) },
  }
  db.$transaction = async callback => callback(db)
  const originalFetch = globalThis.fetch
  globalThis.fetch = async (url, init) => {
    const request = JSON.parse(init.body)
    if (String(url).endsWith('/internal/bridge-saved')) return new Response(JSON.stringify(request), { headers: { 'content-type': 'application/json' } })
    const { method, params, id } = request, isAcrux = String(url).includes(':5042')
    let result
    if (method === 'getCblockProof') result = proof
    else if (method === 'getAccountInfo') result = { context: { slot: 1000 }, value: (isAcrux ? acruxAccounts : accounts).get(params[0]) ?? null }
    else if (method === 'getMultipleAccounts') result = { context: { slot: 1000 }, value: params[0].map(address => (isAcrux ? acruxAccounts : accounts).get(address) ?? null) }
    else if (method === 'getProgramAccounts') result = [...accounts].filter(([, value]) => Buffer.from(value.data[0], 'base64').subarray(0, 8).toString('base64') === params[1].filters[0].memcmp.bytes).map(([pubkey, account]) => ({ pubkey, account }))
    else if (method === 'getBalance') result = { context: { slot: 1000 }, value: lamports }
    else if (method === 'getLatestBlockhash') result = { context: { slot: 1000 }, value: { blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1_000_000 } }
    else if (method === 'getBlockHeight') result = 1000
    else if (method === 'getSignatureStatuses') result = { context: { slot: 1000 }, value: params[0].map(() => null) }
    // A fee payer with no lamports fails simulation as AccountNotFound, without logs.
    else if (method === 'simulateTransaction') result = { context: { slot: 1000 }, value: { err: 'AccountNotFound', logs: [], unitsConsumed: 0 } }
    else throw new Error(`Unexpected RPC ${method}`)
    return new Response(JSON.stringify({ jsonrpc: '2.0', id, result }), { headers: { 'content-type': 'application/json' } })
  }
  const started = Date.now()
  try {
    await createBridgeRelayer({
      config: { program, rpcUrl: 'http://127.0.0.1:5041', gatewayUrl: 'http://127.0.0.1:5042', gatewayInternalUrl: 'http://127.0.0.1:5043', maxSavedCblocksPerCycle: 512, keypairFile: 'unused-in-memory', intervalMs: 15_000, maxNewWithdrawalsPerCycle: 10, maxRetryWithdrawalsPerCycle: 10, maxAccountRetryWithdrawalsPerCycle: 30 },
      signer, db, isLeader: () => true, log: () => {},
    }).runCycle()
  } finally { globalThis.fetch = originalFetch }
  assert.equal(unpaid.size, count, 'Every withdrawal stays recorded unpaid with its proof.')
  return [...unpaid.values()].map(row => (row.nextAttemptAt.getTime() - started) / 1000)
}

const unfunded = await cycle(0)
for (const wait of unfunded) assert.ok(wait <= 61, `An unfunded relayer retries within a minute, not ${wait.toFixed(0)} s.`)
const funded = await cycle(1_000_000_000)
for (const wait of funded) assert.ok(wait >= 119 && wait <= 121, `A funded relayer's other failure keeps the 2 minute first retry, not ${wait.toFixed(0)} s.`)
console.log(`PASS unfunded retry: with 0 SOL, ${count} withdrawals kept and retried in ${Math.max(...unfunded).toFixed(0)} s; funded, the first retry stays ${Math.min(...funded).toFixed(0)} s`)
