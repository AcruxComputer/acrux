// Focused checks for the subtle confirmed/finalized journal boundary; no RPC or servers.
import assert from 'node:assert/strict'
import { createJournal } from '../dist/engines/keeper/journal.js'
import { attestationBatches, pendingFinalityStore } from '../dist/bridge-relayer/transactions.js'

const vote = (validator, kind = 'finalize') => ({ validator, kind, signature: '' })
const proof = { stakes: Array(12).fill('100000000'), votes: Array.from({ length: 12 }, (_, i) => vote(i)) }
const plan = attestationBatches(proof, { finalize: 0, notar: 0 })
assert.equal(plan.kind, 'finalize')
assert.deepEqual(plan.batches.map(batch => batch.length), [4, 4])
const partial = attestationBatches(proof, { finalize: 0b1111, notar: 0 })
assert.deepEqual(partial.batches.flat().map(v => v.validator), [4, 5, 6, 7])
const notar = attestationBatches({ ...proof, votes: [...proof.votes.slice(0, 4), ...Array.from({ length: 12 }, (_, i) => vote(i, 'notar'))] }, { finalize: 0, notar: 0 })
assert.equal(notar.kind, 'notar')
assert.deepEqual(notar.batches.map(batch => batch.length), [4, 4, 2])
console.log('PASS stake planning: two parallel Finalize batches, counted bits skipped, Notar threshold honored')

const original = Buffer.from('original-journaled-bytes')
let row = { key: 'bridge:program:post:hash', state: 'signed', signature: 'original-signature', signedBase64: original.toString('base64'), blockhash: 'original-blockhash', lastValidBlockHeight: 20, contextSlot: 10, error: null }
const stored = {
  get: async () => ({ ...row }),
  put: async () => { throw new Error('A pending transaction must never be rebuilt') },
  update: async (_key, patch) => { row = { ...row, ...patch } },
  pending: async () => [row],
}
const wrapped = pendingFinalityStore(stored)
const statuses = [null, { slot: 12, err: null, confirmationStatus: 'confirmed' }]
const broadcasts = []
const connection = {
  getSignatureStatuses: async () => ({ value: [statuses.shift() ?? null] }),
  getBlockHeight: async () => 12,
  sendRawTransaction: async bytes => { broadcasts.push(Buffer.from(bytes)); return row.signature },
}
const journal = createJournal({ connection, signer: {}, store: wrapped, followMs: 5000 })
assert.equal(await journal.follow(row.key), 'confirmed')
assert.equal(row.state, 'signed', 'confirmed bytes must remain in the durable restart queue')
assert.deepEqual(broadcasts, [original])

// A restart after that confirmed fork disappears still follows those bytes; only the engine's
// finalized blockhash-expiry proof can authorize a replacement.
const restarted = createJournal({ connection: {
  getSignatureStatuses: async () => ({ value: [null] }),
  getBlockHeight: async () => 21,
  isBlockhashValid: async () => ({ value: false }),
  sendRawTransaction: async () => { throw new Error('Expired bytes cannot be broadcast') },
}, signer: {}, store: wrapped, followMs: 5000 })
await assert.rejects(restarted.follow(row.key), /expired/)
assert.equal(row.state, 'expired')
console.log('PASS fork recovery: original bytes survive confirmation/restart; replacement waits for proven expiry')
