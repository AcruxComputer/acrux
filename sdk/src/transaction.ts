// Reads a signed Solana transaction's wire bytes and derives the public header the node derives
// from the same bytes, so the two always agree.

import { sha256 } from '@noble/hashes/sha2.js'
import { bytesToHex } from '@noble/hashes/utils.js'
import { toBase58, type TxHeader } from '@acrux/contract'
import { invalid } from './errors.js'

/** A signed `Transaction` or `VersionedTransaction` from @solana/web3.js, or its serialized bytes. */
export type SignedTransaction = Uint8Array | { serialize(): Uint8Array }

const COMPUTE_BUDGET = 'ComputeBudget111111111111111111111111111111'
const UPGRADEABLE_LOADER = 'BPFLoaderUpgradeab1e11111111111111111111111'

export function transactionBytes(transaction: SignedTransaction): Uint8Array {
  return Uint8Array.from(transaction instanceof Uint8Array ? transaction : transaction.serialize())
}

export function deriveHeader(bytes: Uint8Array, targetSlot: number): TxHeader {
  let at = 0
  const take = (length: number) => {
    if (at + length > bytes.length) throw invalid('Not a signed Solana transaction.')
    return bytes.subarray(at, (at += length))
  }
  const compact = () => {
    let value = 0
    for (let shift = 0; ; shift += 7) {
      const byte = take(1)[0]!
      value |= (byte & 0x7f) << shift
      if (!(byte & 0x80)) return value
    }
  }

  take(compact() * 64)
  const message = bytes.subarray(at)
  if (take(1)[0]! < 0x80) at -= 1 // a legacy message has no version byte
  const [signers, readonlySigned, readonlyUnsigned] = take(3)
  const keys = Array.from({ length: compact() }, () => toBase58(take(32)))
  take(32)

  let limit: number | undefined
  let price = 0n
  let instructions = 0
  const programs = new Set<number>()
  for (let count = compact(); count > 0; count--) {
    const program = take(1)[0]!
    take(compact())
    const data = take(compact())
    programs.add(program)
    if (keys[program] !== COMPUTE_BUDGET) instructions += 1
    else {
      const view = new DataView(data.buffer, data.byteOffset, data.length)
      if (data[0] === 2 && data.length === 5) limit = view.getUint32(1, true)
      if (data[0] === 3 && data.length === 9) price = view.getBigUint64(1, true)
    }
  }

  const demotePrograms = !keys.includes(UPGRADEABLE_LOADER)
  const writable = (i: number) =>
    (i < signers! - readonlySigned! || (i >= signers! && i < keys.length - readonlyUnsigned!)) &&
    !(demotePrograms && programs.has(i))
  if (!keys.length) throw invalid('Not a signed Solana transaction.')

  return {
    id: bytesToHex(sha256(message)),
    feePayer: keys[0]!,
    priorityFeePerCu: price.toString(),
    computeLimit: limit ?? Math.min(instructions * 200_000, 1_400_000),
    writableAccounts: keys.filter((_, i) => writable(i)),
    targetSlot,
  }
}

/** The bytes the fee payer signs, and the associated data of the threshold ciphertext. */
export function headerBytes(header: TxHeader): Uint8Array {
  const { id, feePayer, priorityFeePerCu, computeLimit, writableAccounts, targetSlot } = header
  return new TextEncoder().encode(JSON.stringify({ id, feePayer, priorityFeePerCu, computeLimit, writableAccounts, targetSlot }))
}
