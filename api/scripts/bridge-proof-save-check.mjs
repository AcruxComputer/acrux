// M7 regression: one real service cycle saves 2,000 finalized withdrawals in 400 cblocks
// while its independent payment budget remains small. RPC and Prisma boundaries stay in
// process; Anchor builders, signing, proof validation and keeper scheduling remain real.
// Run after npm.cmd run build: node scripts/bridge-proof-save-check.mjs
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { ACRUX_BRIDGE_IDL } from '@acrux/contract'
import { BorshAccountsCoder, BorshInstructionCoder } from '@coral-xyz/anchor'
import BN from 'bn.js'
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { Keypair, PublicKey, SystemProgram, TransactionMessage, VersionedTransaction } from '@solana/web3.js'
import { createBridgeRelayer } from '../dist/bridge-relayer/service.js'
import { readBridgeRelayerConfig } from '../dist/bridge-relayer/config.js'
import { signatureOf } from '../dist/engines/tx/server/ed25519.js'

const program = new PublicKey(ACRUX_BRIDGE_IDL.address)
const signer = Keypair.fromSeed(Buffer.alloc(32, 7))
const other = Keypair.fromSeed(Buffer.alloc(32, 8)).publicKey
const mint = Keypair.fromSeed(Buffer.alloc(32, 9)).publicKey
const recipient = Keypair.fromSeed(Buffer.alloc(32, 10)).publicKey
const epochSlots = 2000, firstNew = 0, withdrawalCount = 2000, cblockCount = 400, newLimit = 3, retryLimit = 2
const chainId = 'acrux-proof-save-regression'
const sha256 = (...parts) => createHash('sha256').update(Buffer.concat(parts)).digest()
const u64 = value => { const bytes = Buffer.alloc(8); bytes.writeBigUInt64LE(BigInt(value)); return bytes }
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const setPda = epoch => pda(Buffer.from('set'), u64(epoch))
const cblockPda = proof => pda(Buffer.from('cblock'), Buffer.from(proof.hash, 'hex'))
const releasedPda = sequence => pda(Buffer.from('released'), u64(Math.floor(sequence / 8192)))
const coder = new BorshAccountsCoder(ACRUX_BRIDGE_IDL)
const instructions = new BorshInstructionCoder(ACRUX_BRIDGE_IDL)
const accounts = new Map(), acruxAccounts = new Map(), actions = [], logs = []
const response = (id, result) => new Response(JSON.stringify({ jsonrpc: '2.0', id, result }), { headers: { 'content-type': 'application/json' } })
const account = (data, owner = program) => ({ executable: false, owner: owner.toBase58(), lamports: 10_000_000, rentEpoch: 0, data: [Buffer.from(data).toString('base64'), 'base64'] })
const encoded = async (name, value) => account(await coder.encode(name, value))
const setValue = epoch => ({ epoch: new BN(epoch), total: new BN(1), validators: [{ key: other, stake: new BN(1) }], payer: other })
const withdrawalEntry = sequence => Buffer.concat([Buffer.from([1, 0, 0, 0]), u64(sequence), mint.toBuffer(), recipient.toBuffer(), u64(1)])
function makeProof(slot, sequences) {
  const epoch = Math.floor(slot * 4 / epochSlots)
  const entries = [Buffer.concat([Buffer.alloc(4), u64(epoch + 1), u64(1), other.toBuffer(), u64(1)]), ...sequences.map(withdrawalEntry)]
  let level = entries.map(entry => sha256(entry))
  while (level.length > 1) level = Array.from({ length: Math.ceil(level.length / 2) }, (_, i) => i * 2 + 1 < level.length ? sha256(level[i * 2], level[i * 2 + 1]) : level[i * 2])
  const preimage = Buffer.concat([u64(slot), Buffer.from('cycle-limit-cblock')])
  return { consensusSlot: slot, chainId, epochSlots, preimage: preimage.toString('base64'), outboxRoot: level[0].toString('hex'), hash: sha256(preimage, level[0]).toString('hex'), outbox: entries.map(entry => entry.toString('base64')), validators: [other.toBase58()], stakes: ['1'], votes: [] }
}
const proofs = new Map(Array.from({ length: cblockCount }, (_, index) => [index + 1, makeProof(index + 1, Array.from({ length: 5 }, (_, item) => index * 5 + item))]))
const oldProof = proofs.get(1)
const latestProof = makeProof(501, []) // Epoch 1 passed, and Solana starts with only set 0.
for (const proof of proofs.values()) accounts.set(cblockPda(proof).toBase58(), await encoded('Cblock', {
  slot: new BN(proof.consensusSlot), epoch: new BN(0), hash: [...Buffer.from(proof.hash, 'hex')], outbox_root: [...Buffer.from(proof.outboxRoot, 'hex')], notar: 0, finalize: 1, certified: true, payer: other,
}))
accounts.set(pda(Buffer.from('bridge')).toBase58(), await encoded('Bridge', { deposits: new BN(0), bump: 0, authority: other, epoch_slots: new BN(epochSlots), first_epoch: new BN(0), seeded: true, chain_id: [...sha256(Buffer.from(chainId))] }))
accounts.set(setPda(0).toBase58(), await encoded('ValidatorSet', setValue(0)))
acruxAccounts.set(pda(Buffer.from('outbound')).toBase58(), await encoded('Outbound', { withdrawals: new BN(withdrawalCount), bump: 0 }))
for (let sequence = 0; sequence < withdrawalCount; sequence++) acruxAccounts.set(pda(Buffer.from('withdrawal'), u64(sequence)).toBase58(), await encoded('Withdrawal', {
  sequence: new BN(sequence), owner: signer.publicKey, recipient, solana_mint: mint, amount: new BN(1), slot: new BN((Math.floor(sequence / 5) + 1) * 4), bump: 0,
}))
const mintData = Buffer.alloc(82)
mintData.writeUInt32LE(1, 0); other.toBuffer().copy(mintData, 4)
mintData.writeBigUInt64LE(1_000_000n, 36); mintData[44] = 6; mintData[45] = 1
accounts.set(mint.toBase58(), account(mintData, TOKEN_PROGRAM_ID))
const destination = getAssociatedTokenAddressSync(mint, recipient, true, TOKEN_PROGRAM_ID)
const destinationData = Buffer.alloc(165)
mint.toBuffer().copy(destinationData, 0); recipient.toBuffer().copy(destinationData, 32); destinationData[108] = 1
accounts.set(destination.toBase58(), account(destinationData, TOKEN_PROGRAM_ID))

const unpaid = new Map(), savedProofs = new Map(), keeperRows = new Map(), acceptedSignatures = new Set()
const proofReads = new Map(), acknowledgements = []
let inTransaction = false, committedCursor = 0n, commits = 0, fullyAcknowledged = false
const state = { program: program.toBase58(), wallet: signer.publicKey.toBase58(), lamports: '1000000000', nextWithdrawalSequence: BigInt(firstNew), savedWithdrawalSequence: BigInt(firstNew), lastCblock: null, lastHandedEpoch: 0n, lastRelayedAt: null }
const matches = (row, where = {}) => Object.entries(where).every(([key, expected]) => {
  if (key === 'AND') return expected.every(condition => matches(row, condition))
  if (key === 'OR') return expected.some(condition => matches(row, condition))
  if (key === 'NOT') return !(Array.isArray(expected) ? expected.every(condition => matches(row, condition)) : matches(row, expected))
  const value = row[key]
  if (expected !== null && typeof expected === 'object' && !(expected instanceof Date)) return Object.entries(expected).every(([operator, limit]) => {
    if (operator === 'startsWith') return value.startsWith(limit)
    if (operator === 'lte') return value <= limit
    if (operator === 'lt') return value < limit
    if (operator === 'gte') return value >= limit
    if (operator === 'gt') return value > limit
    if (operator === 'in') return limit.includes(value)
    if (operator === 'notIn') return !limit.includes(value)
    if (operator === 'not') return value !== limit
    throw new Error(`Unsupported Prisma comparison ${operator}`)
  })
  return value === expected
})
function rows(values, args = {}) {
  const result = [...values].filter(row => matches(row, args.where))
  const order = Array.isArray(args.orderBy) ? args.orderBy : args.orderBy ? [args.orderBy] : []
  result.sort((a, b) => {
    for (const fields of order) for (const [key, direction] of Object.entries(fields)) {
      const difference = a[key] < b[key] ? -1 : a[key] > b[key] ? 1 : 0
      if (difference) return direction === 'asc' ? difference : -difference
    }
    return 0
  })
  const page = result.slice(args.skip ?? 0, args.take === undefined ? undefined : (args.skip ?? 0) + args.take)
  return args.select ? page.map(row => Object.fromEntries(Object.entries(args.select).filter(([, keep]) => keep).map(([key]) => [key, row[key]]))) : page
}
const sequenceOf = where => Number(where.program_sequence.sequence)
const db = {
  bridgeRelayerState: {
    upsert: async ({ update }) => {
      if (update.savedWithdrawalSequence !== undefined) assert.ok(inTransaction, 'The saved cursor must change inside the proof transaction.')
      return Object.assign(state, update)
    },
    findUniqueOrThrow: async () => state,
    findUnique: async () => state,
    update: async ({ data }) => {
      if (data.savedWithdrawalSequence !== undefined) {
        assert.ok(inTransaction, 'The saved cursor must be committed with its proof.')
        assert.ok(data.savedWithdrawalSequence >= state.savedWithdrawalSequence)
        if (data.savedWithdrawalSequence > 0n) assert.ok(savedProofs.has(Math.floor((Number(data.savedWithdrawalSequence) - 1) / 5) + 1), 'The cursor cannot precede its proof write.')
      }
      return Object.assign(state, data)
    },
  },
  bridgeSavedCblockProof: {
    findUnique: async ({ where }) => savedProofs.get(Number(where.program_cblock.cblock)) ?? null,
    upsert: async ({ where, create, update }) => {
      assert.ok(inTransaction, 'Proof persistence must use the transaction that advances the saved cursor.')
      const cblock = Number(where.program_cblock.cblock)
      if (!savedProofs.has(cblock)) savedProofs.set(cblock, { ...create })
      else Object.assign(savedProofs.get(cblock), update)
      return savedProofs.get(cblock)
    },
  },
  bridgeUnpaidWithdrawal: {
    findUnique: async ({ where }) => unpaid.get(sequenceOf(where)) ?? null,
    findMany: async args => rows(unpaid.values(), args),
    upsert: async ({ where, create, update }) => {
      const sequence = sequenceOf(where)
      if (!unpaid.has(sequence)) unpaid.set(sequence, { ...create })
      else Object.assign(unpaid.get(sequence), update)
      return unpaid.get(sequence)
    },
    update: async ({ where, data }) => {
      const sequence = sequenceOf(where)
      assert.ok(unpaid.has(sequence))
      if (data.attempts !== undefined) actions.push({ kind: 'payout', sequence })
      return Object.assign(unpaid.get(sequence), data)
    },
    deleteMany: async ({ where }) => {
      let count = 0
      for (const [sequence, row] of unpaid) if (matches(row, where)) { unpaid.delete(sequence); count++ }
      return { count }
    },
  },
  keeperTransaction: {
    findUnique: async ({ where }) => keeperRows.get(where.key) ?? null,
    findMany: async args => rows(keeperRows.values(), args),
    create: async ({ data }) => { assert.ok(!keeperRows.has(data.key)); keeperRows.set(data.key, { ...data, createdAt: new Date() }); return data },
    updateMany: async ({ where, data }) => {
      let count = 0
      for (const row of keeperRows.values()) if (matches(row, where)) { Object.assign(row, data); count++ }
      return { count }
    },
  },
}

db.$transaction = async callback => {
  assert.equal(inTransaction, false)
  inTransaction = true
  try {
    const result = await callback(db)
    committedCursor = state.savedWithdrawalSequence
    commits++
    return result
  } finally { inTransaction = false }
}

const originalFetch = globalThis.fetch
try {
  globalThis.fetch = async (url, init) => {
    const request = JSON.parse(init.body)
    if (String(url).endsWith('/internal/bridge-saved')) {
      assert.equal(inTransaction, false, 'The node acknowledgement must follow the database commit.')
      assert.equal(request.program, program.toBase58())
      assert.match(request.nextWithdrawalSequence, /^(0|[1-9]\d*)$/)
      const next = BigInt(request.nextWithdrawalSequence)
      assert.ok(next <= committedCursor, 'The node cannot shed proofs above the committed saved cursor.')
      for (let sequence = 0; sequence < Number(next); sequence++) assert.ok(savedProofs.has(Math.floor(sequence / 5) + 1))
      if (next > 0n) assert.ok(commits > 0)
      acknowledgements.push(next)
      fullyAcknowledged = next === BigInt(withdrawalCount)
      actions.push({ kind: 'ack', sequence: Number(next) })
      return new Response(JSON.stringify({ program: request.program, nextWithdrawalSequence: request.nextWithdrawalSequence }), { headers: { 'content-type': 'application/json' } })
    }
    const { method, params, id } = request
    const isAcrux = String(url).includes(':5042')
    let result
    if (method === 'getCblockProof') {
      if (params.latest) result = latestProof
      else if (params.epoch === 0) result = oldProof
      else {
        assert.equal(fullyAcknowledged, false, 'Payouts must read their saved PostgreSQL proof, without a gateway fallback.')
        result = proofs.get(params.consensusSlot)
        proofReads.set(params.consensusSlot, (proofReads.get(params.consensusSlot) ?? 0) + 1)
      }
      assert.ok(result, 'Unexpected gateway proof request')
    } else if (method === 'getAccountInfo') result = { context: { slot: isAcrux ? latestProof.consensusSlot * 4 + 3 : 1000 }, value: (isAcrux ? acruxAccounts : accounts).get(params[0]) ?? null }
    else if (method === 'getMultipleAccounts') result = { context: { slot: isAcrux ? latestProof.consensusSlot * 4 + 3 : 1000 }, value: params[0].map(address => (isAcrux ? acruxAccounts : accounts).get(address) ?? null) }
    else if (method === 'getProgramAccounts') {
      const discriminator = params[1].filters[0].memcmp.bytes
      result = [...accounts].filter(([, value]) => Buffer.from(value.data[0], 'base64').subarray(0, 8).toString('base64') === discriminator).map(([pubkey, account]) => ({ pubkey, account }))
    } else if (method === 'getBalance') result = { context: { slot: 1000 }, value: 1_000_000_000 }
    else if (method === 'getLatestBlockhash') result = { context: { slot: 1000 }, value: { blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1_000_000 } }
    else if (method === 'getBlockHeight') result = 1000
    else if (method === 'getSignatureStatuses') result = { context: { slot: 1000 }, value: params[0].map(signature => acceptedSignatures.has(signature) ? { slot: 1000, confirmations: null, err: null, confirmationStatus: 'finalized' } : null) }
    else if (method === 'getTransaction') result = null
    else if (method === 'simulateTransaction' || method === 'sendTransaction') {
      const transaction = VersionedTransaction.deserialize(Buffer.from(params[0], 'base64'))
      const decoded = transaction.message.compiledInstructions.filter(ix => transaction.message.staticAccountKeys[ix.programIdIndex].equals(program)).map(ix => instructions.decode(Buffer.from(ix.data)))
      assert.ok(decoded.length > 0)
      if (decoded.some(ix => ix.name === 'release')) assert.equal(fullyAcknowledged, true, 'The independent save pass must finish before payouts run.')
      if (method === 'simulateTransaction') result = { context: { slot: 1000 }, value: decoded.some(ix => ix.name === 'release')
        ? { err: { InstructionError: [2, { Custom: 1 }] }, logs: ['Program log: destination owner does not match'], unitsConsumed: 10_000 }
        : { err: null, logs: [], unitsConsumed: 10_000 } }
      else {
        for (const instruction of decoded) {
          assert.equal(instruction.name, 'hand_off', 'Only the epoch hand-off succeeds in this check.')
          actions.push({ kind: 'handoff', epoch: instruction.data.epoch.toNumber() })
          accounts.set(setPda(instruction.data.epoch.toNumber()).toBase58(), await encoded('ValidatorSet', { ...setValue(instruction.data.epoch.toNumber()), payer: signer.publicKey }))
        }
        result = signatureOf(transaction); acceptedSignatures.add(result)
      }
    } else throw new Error(`Unexpected RPC ${method}`)
    return response(id, result)
  }
  const config = readBridgeRelayerConfig({
    BRIDGE_PROGRAM_ID: program.toBase58(), SOLANA_RPC_URL: 'http://127.0.0.1:5041', GATEWAY_RPC_URL: 'http://127.0.0.1:5042', GATEWAY_INTERNAL_URL: 'http://127.0.0.1:5043', BRIDGE_RELAYER_KEYPAIR_FILE: 'unused-in-memory',
    BRIDGE_RELAYER_MAX_NEW_WITHDRAWALS_PER_CYCLE: String(newLimit), BRIDGE_RELAYER_MAX_RETRY_WITHDRAWALS_PER_CYCLE: String(retryLimit),
  })
  assert.equal(config.maxSavedCblocksPerCycle, 512, 'The default cblock save budget must cover this live-rate backlog.')
  const relayer = createBridgeRelayer({ config, signer, db, isLeader: () => true, log: entry => logs.push(entry) })
  await relayer.runCycle()
} finally { globalThis.fetch = originalFetch }

assert.equal(actions[0]?.kind, 'handoff', 'The light client must hand on before proof saving and payouts.')
assert.equal(actions[0].epoch, 1)
assert.equal(state.savedWithdrawalSequence, 2000n, 'One save cycle must cover all 2,000 finalized withdrawals.')
assert.equal(savedProofs.size, 400, 'Persist one proof for each cblock rather than one copy per withdrawal.')
assert.equal(proofReads.size, 400)
for (const [cblock, count] of proofReads) assert.equal(count, 1, `Cblock ${cblock}: fetch its shared proof once.`)
assert.equal(acknowledgements.at(-1), 2000n)
assert.equal(fullyAcknowledged, true)
const attempted = actions.filter(action => action.kind === 'payout').map(action => action.sequence)
assert.deepEqual(attempted, [0, 1, 2], 'Saving 2,000 withdrawals must not raise the payment budget.')
assert.equal(state.nextWithdrawalSequence, 3n)
assert.ok(state.nextWithdrawalSequence < state.savedWithdrawalSequence)
for (let sequence = 0; sequence < withdrawalCount; sequence++) assert.ok(savedProofs.has(Math.floor(sequence / 5) + 1), `Withdrawal ${sequence}: its proof must outlive gateway retention.`)
assert.ok(logs.some(entry => entry.event === 'bridge_handed_on'))
console.log('PASS proof save: 2,000 finalized withdrawals / 400 shared cblocks saved before acknowledgement, 3 payments attempted from PostgreSQL')
