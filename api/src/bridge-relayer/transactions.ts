import type { KeeperTxStore } from '../engines/keeper/journal.js'
import type { Proof } from './proof.js'

/** The bridge can use confirmed writes, but their original bytes stay resumable until finality. */
export function pendingFinalityStore(store: KeeperTxStore): KeeperTxStore {
  return {
    ...store,
    async update(key, patch) {
      if (patch.state === 'confirmed') return
      await store.update(key, patch)
    },
  }
}

type VoteBits = { finalize: number; notar: number }

/** Send just enough additional votes of one kind, in independent batches of at most four. */
export function attestationBatches(proof: Proof, bits: VoteBits) {
  const total = proof.stakes.reduce((sum, stake) => sum + BigInt(stake), 0n)
  for (const kind of ['finalize', 'notar'] as const) {
    const percent = kind === 'finalize' ? 60n : 80n
    let stake = proof.stakes.reduce((sum, weight, index) => sum + ((bits[kind] >>> index & 1) ? BigInt(weight) : 0n), 0n)
    const available = proof.votes.filter((vote, index, all) => vote.kind === kind && !(bits[kind] >>> vote.validator & 1) && all.findIndex(v => v.kind === kind && v.validator === vote.validator) === index)
    const selected: Proof['votes'] = []
    for (const vote of available) {
      if (stake * 100n >= total * percent) break
      selected.push(vote)
      stake += BigInt(proof.stakes[vote.validator])
    }
    if (stake * 100n < total * percent) continue
    const batches: Proof['votes'][] = []
    for (let index = 0; index < selected.length; index += 4) batches.push(selected.slice(index, index + 4))
    return { kind, batches }
  }
  throw new Error('Gateway proof does not carry enough stake to certify its cblock.')
}
