// M5 regression: one real relayer cycle crosses an epoch boundary with a large scan and
// retry backlog. Only the HTTP RPC boundary and Prisma persistence are in memory. Anchor
// instruction builders, transaction signing, journal reconciliation and scheduling are real.
// Run after npm.cmd run build: node scripts/bridge-cycle-limits-check.mjs
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { ACRUX_BRIDGE_IDL } from '@acrux/contract'
import { BorshAccountsCoder, BorshInstructionCoder, Program } from '@coral-xyz/anchor'
import BN from 'bn.js'
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { Connection, Keypair, PublicKey, SystemProgram, TransactionMessage, VersionedTransaction } from '@solana/web3.js'
import { createBridgeRelayer } from '../dist/bridge-relayer/service.js'
import { outboxProof } from '../dist/bridge-relayer/proof.js'
import { signatureOf } from '../dist/engines/tx/server/ed25519.js'

const program = new PublicKey(ACRUX_BRIDGE_IDL.address)
const signer = Keypair.fromSeed(Buffer.alloc(32, 7))
const other = Keypair.fromSeed(Buffer.alloc(32, 8)).publicKey
const mint = Keypair.fromSeed(Buffer.alloc(32, 9)).publicKey
const recipient = Keypair.fromSeed(Buffer.alloc(32, 10)).publicKey
const epochSlots = 400, firstNew = 20, withdrawalCount = 50, newLimit = 3, retryLimit = 2
const chainId = 'acrux-cycle-limits-regression'
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
const oldProof = makeProof(90, Array.from({ length: firstNew }, (_, i) => i))
const newProof = makeProof(101, Array.from({ length: withdrawalCount - firstNew }, (_, i) => firstNew + i))
const latestProof = makeProof(150, []) // Epoch 1 has finalized, while Solana still has only set 0.
for (const proof of [oldProof, newProof]) accounts.set(cblockPda(proof).toBase58(), await encoded('Cblock', {
  slot: new BN(proof.consensusSlot), epoch: new BN(Math.floor(proof.consensusSlot * 4 / epochSlots)), hash: [...Buffer.from(proof.hash, 'hex')], outbox_root: [...Buffer.from(proof.outboxRoot, 'hex')], notar: 0, finalize: 1, certified: true, payer: other,
}))
accounts.set(pda(Buffer.from('bridge')).toBase58(), await encoded('Bridge', { deposits: new BN(0), bump: 0, authority: other, epoch_slots: new BN(epochSlots), first_epoch: new BN(0), seeded: true, chain_id: [...sha256(Buffer.from(chainId))] }))
accounts.set(setPda(0).toBase58(), await encoded('ValidatorSet', setValue(0)))
acruxAccounts.set(pda(Buffer.from('outbound')).toBase58(), await encoded('Outbound', { withdrawals: new BN(withdrawalCount), bump: 0 }))
for (let sequence = 0; sequence < withdrawalCount; sequence++) acruxAccounts.set(pda(Buffer.from('withdrawal'), u64(sequence)).toBase58(), await encoded('Withdrawal', {
  sequence: new BN(sequence), owner: signer.publicKey, recipient, solana_mint: mint, amount: new BN(1), slot: new BN((sequence < firstNew ? oldProof : newProof).consensusSlot * 4), bump: 0,
}))
const mintData = Buffer.alloc(82)
mintData.writeUInt32LE(1, 0); other.toBuffer().copy(mintData, 4)
mintData.writeBigUInt64LE(1_000_000n, 36); mintData[44] = 6; mintData[45] = 1
accounts.set(mint.toBase58(), account(mintData, TOKEN_PROGRAM_ID))
const destination = getAssociatedTokenAddressSync(mint, recipient, true, TOKEN_PROGRAM_ID)
const destinationData = Buffer.alloc(165)
mint.toBuffer().copy(destinationData, 0); recipient.toBuffer().copy(destinationData, 32); destinationData[108] = 1
accounts.set(destination.toBase58(), account(destinationData, TOKEN_PROGRAM_ID))

const unpaid = new Map(Array.from({ length: firstNew }, (_, sequence) => [sequence, {
  program: program.toBase58(), sequence: BigInt(sequence), cblock: BigInt(oldProof.consensusSlot), epoch: 0n,
  proof: oldProof, reason: 'Unpayable destination', attempts: 3,
  // Sequence order is deliberately different from retry fairness order.
  nextAttemptAt: new Date((firstNew - sequence) * 1000),
}]))
const savedProofs = new Map([[oldProof.consensusSlot, { program: program.toBase58(), cblock: BigInt(oldProof.consensusSlot), proof: oldProof }]])
const keeperRows = new Map(), oldSignedBytes = new Map(), pendingSignatures = new Map(), acceptedSignatures = new Set()
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
    upsert: async ({ update }) => Object.assign(state, update),
    findUniqueOrThrow: async () => state,
    findUnique: async () => state,
    update: async ({ data }) => Object.assign(state, data),
  },
  bridgeSavedCblockProof: {
    findUnique: async ({ where }) => savedProofs.get(Number(where.program_cblock.cblock)) ?? null,
    upsert: async ({ where, create, update }) => {
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

db.$transaction = async callback => callback(db)

// Real signed release rows ensure startup reconciliation cannot evade the retry allowance.
const builder = new Program(ACRUX_BRIDGE_IDL, { connection: new Connection('http://127.0.0.1:5041') })
for (let sequence = 0; sequence < firstNew; sequence++) {
  const instruction = await builder.methods.release(new BN(sequence), recipient, new BN(1), outboxProof(oldProof.outbox.map(entry => Buffer.from(entry, 'base64')), sequence + 1)).accountsStrict({
    payer: signer.publicKey, cblock: cblockPda(oldProof), mint, vault: pda(Buffer.from('vault'), mint.toBuffer()), destination,
    released: releasedPda(sequence), tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).instruction()
  const transaction = new VersionedTransaction(new TransactionMessage({ payerKey: signer.publicKey, recentBlockhash: PublicKey.default.toBase58(), instructions: [instruction] }).compileToV0Message())
  transaction.sign([signer])
  const signature = signatureOf(transaction), key = `bridge:${program.toBase58()}:release:${sequence}`
  const row = { key, state: 'signed', signature, signedBase64: Buffer.from(transaction.serialize()).toString('base64'), blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1_000_000n, contextSlot: 1000n, error: null, createdAt: new Date(sequence * 1000) }
  keeperRows.set(key, row); oldSignedBytes.set(sequence, row.signedBase64); pendingSignatures.set(signature, sequence)
}

const originalFetch = globalThis.fetch
try {
  globalThis.fetch = async (url, init) => {
    const request = JSON.parse(init.body)
    if (String(url).endsWith('/internal/bridge-saved')) {
      assert.equal(request.program, program.toBase58())
      assert.equal(request.nextWithdrawalSequence, state.savedWithdrawalSequence.toString())
      return new Response(JSON.stringify({ program: request.program, nextWithdrawalSequence: request.nextWithdrawalSequence }), { headers: { 'content-type': 'application/json' } })
    }
    const { method, params, id } = request
    const isAcrux = String(url).includes(':5042')
    let result
    if (method === 'getCblockProof') {
      result = params.latest ? latestProof : params.epoch === 0 || params.consensusSlot === oldProof.consensusSlot ? oldProof : params.consensusSlot === newProof.consensusSlot ? newProof : null
      assert.ok(result, 'Unexpected gateway proof request')
    } else if (method === 'getAccountInfo') result = { context: { slot: 1000 }, value: (isAcrux ? acruxAccounts : accounts).get(params[0]) ?? null }
    else if (method === 'getMultipleAccounts') result = { context: { slot: 1000 }, value: params[0].map(address => (isAcrux ? acruxAccounts : accounts).get(address) ?? null) }
    else if (method === 'getProgramAccounts') {
      const discriminator = params[1].filters[0].memcmp.bytes
      result = [...accounts].filter(([, value]) => Buffer.from(value.data[0], 'base64').subarray(0, 8).toString('base64') === discriminator).map(([pubkey, account]) => ({ pubkey, account }))
    } else if (method === 'getBalance') result = { context: { slot: 1000 }, value: 1_000_000_000 }
    else if (method === 'getLatestBlockhash') result = { context: { slot: 1000 }, value: { blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1_000_000 } }
    else if (method === 'getBlockHeight') result = 1000
    else if (method === 'getSignatureStatuses') result = { context: { slot: 1000 }, value: params[0].map(signature => {
      if (pendingSignatures.has(signature)) {
        actions.push({ kind: 'reconcile-release', sequence: pendingSignatures.get(signature) })
        return { slot: 1000, confirmations: null, err: { InstructionError: [0, { Custom: 1 }] }, confirmationStatus: 'finalized' }
      }
      return acceptedSignatures.has(signature) ? { slot: 1000, confirmations: null, err: null, confirmationStatus: 'finalized' } : null
    }) }
    else if (method === 'getTransaction') result = null
    else if (method === 'simulateTransaction' || method === 'sendTransaction') {
      const transaction = VersionedTransaction.deserialize(Buffer.from(params[0], 'base64'))
      const decoded = transaction.message.compiledInstructions.filter(ix => transaction.message.staticAccountKeys[ix.programIdIndex].equals(program)).map(ix => instructions.decode(Buffer.from(ix.data)))
      assert.ok(decoded.length > 0)
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
  const relayer = createBridgeRelayer({
    config: { program, rpcUrl: 'http://127.0.0.1:5041', gatewayUrl: 'http://127.0.0.1:5042', gatewayInternalUrl: 'http://127.0.0.1:5043', maxSavedCblocksPerCycle: 512, keypairFile: 'unused-in-memory', intervalMs: 15_000, maxNewWithdrawalsPerCycle: newLimit, maxRetryWithdrawalsPerCycle: retryLimit },
    signer, db, isLeader: () => true, log: entry => logs.push(entry),
  })
  await relayer.runCycle()
} finally { globalThis.fetch = originalFetch }

assert.equal(actions[0]?.kind, 'handoff', 'The epoch must hand on before any new payout, retry, or signed-release reconciliation.')
assert.equal(actions[0].epoch, 1)
const attempted = actions.filter(action => action.kind === 'payout').map(action => action.sequence)
assert.deepEqual(attempted.filter(sequence => sequence >= firstNew), [20, 21, 22], 'One proof containing many withdrawals must not bypass the new scan limit.')
assert.deepEqual(attempted.filter(sequence => sequence < firstNew), [19, 18], 'Only the two oldest due retries may run, ordered by nextAttemptAt before sequence.')
assert.deepEqual(attempted, [20, 21, 22, 19, 18], 'New withdrawals must run before the bounded retry queue.')
assert.equal(state.nextWithdrawalSequence, BigInt(firstNew + newLimit), 'The persisted cursor must leave the remaining backlog for the next cycle.')
assert.ok(state.nextWithdrawalSequence < BigInt(withdrawalCount))
assert.equal(state.savedWithdrawalSequence, BigInt(withdrawalCount), 'The independent save pass must cover the full proof, ahead of the payout cursor.')
assert.ok(savedProofs.has(newProof.consensusSlot))
for (let sequence = 0; sequence < firstNew; sequence++) {
  const row = unpaid.get(sequence)
  assert.equal(row.attempts, [19, 18].includes(sequence) ? 4 : 3)
  if (![19, 18].includes(sequence)) {
    const journal = keeperRows.get(`bridge:${program.toBase58()}:release:${sequence}`)
    assert.equal(journal.state, 'signed', 'Unselected signed releases must stay resumable without consuming work this cycle.')
    assert.equal(journal.signedBase64, oldSignedBytes.get(sequence))
  }
}
for (let sequence = firstNew + newLimit; sequence < withdrawalCount; sequence++) assert.equal(unpaid.has(sequence), false, 'Unselected neighboring proof entries must not be attempted or enqueued.')
assert.ok(logs.some(entry => entry.event === 'bridge_handed_on'))
console.log('PASS cycle limits: hand-off first, 3 new withdrawals, 2 oldest due retries, shared-proof and signed-row budgets preserved')
