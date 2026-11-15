// The threshold transaction: ChaCha20-Poly1305 under a fresh key, the ciphertext erasure-coded into
// n shards of which any t rebuild it, the key Shamir-split the same way. Byte-compatible with the
// node's `seal_for_receivers` and its decoder.

import { chacha20poly1305 } from '@noble/ciphers/chacha.js'
import { fromBase64, toBase58, toBase64, type FragmentSubmission, type TxHeader } from '@acrux/contract'
import { invalid } from './errors.js'
import { headerBytes } from './transaction.js'

type Mul = (a: number, b: number) => number

/** Multiplication in GF(256) reduced by x^8 plus the given low bits. */
const field = (reduce: number): Mul => (a, b) => {
  let product = 0
  for (; b; b >>= 1) {
    if (b & 1) product ^= a
    a = a & 0x80 ? ((a << 1) ^ reduce) & 0xff : a << 1
  }
  return product
}
/** The Reed-Solomon field of the `reed-solomon-erasure` crate's galois_8 (0x11d). */
const rs = field(0x1d)
/** The Shamir field of the node (0x11b). */
const shamir = field(0x1b)

const power = (mul: Mul, a: number, n: number) => {
  let result = 1
  while (n-- > 0) result = mul(result, a)
  return result
}
const inverse = (mul: Mul, a: number) => power(mul, a, 254)

const times = (a: number[][], b: number[][]) =>
  a.map(row => b[0]!.map((_, c) => row.reduce((sum, value, k) => sum ^ rs(value, b[k]![c]!), 0)))

/** Gauss-Jordan inverse of a square matrix over the Reed-Solomon field. */
function invert(matrix: number[][]): number[][] {
  const size = matrix.length
  const work = matrix.map((row, r) => [...row, ...row.map((_, c) => (r === c ? 1 : 0))])
  for (let c = 0; c < size; c++) {
    const pivot = work.findIndex((row, r) => r >= c && row[c] !== 0)
    ;[work[c], work[pivot]] = [work[pivot]!, work[c]!]
    const scale = inverse(rs, work[c]![c]!)
    work[c] = work[c]!.map(value => rs(value, scale))
    for (let r = 0; r < size; r++) {
      const factor = work[r]![c]!
      if (r !== c && factor) work[r] = work[r]!.map((value, k) => value ^ rs(factor, work[c]![k]!))
    }
  }
  return work.map(row => row.slice(size))
}

/** The crate's systematic matrix: a Vandermonde matrix times the inverse of its top square. Row i makes shard i. */
function encodingMatrix(t: number, n: number): number[][] {
  const vandermonde = Array.from({ length: n }, (_, r) => Array.from({ length: t }, (_, c) => power(rs, r, c)))
  return times(vandermonde, invert(vandermonde.slice(0, t)))
}

const combine = (rows: number[][], shards: Uint8Array[]) =>
  rows.map(row => shards[0]!.map((_, j) => row.reduce((sum, factor, k) => sum ^ rs(factor, shards[k]![j]!), 0)))

/**
 * Seals a signed transaction into n fragments, one per receiver, readable from any t of them.
 * `headerSignature` is the fee payer's Ed25519 signature over `headerBytes(header)`.
 */
export function seal(transaction: Uint8Array, header: TxHeader, headerSignature: Uint8Array, t: number, receivers: number[]): FragmentSubmission[] {
  const n = receivers.length
  const key = crypto.getRandomValues(new Uint8Array(32))
  const nonce = crypto.getRandomValues(new Uint8Array(12))
  const ciphertext = chacha20poly1305(key, nonce, headerBytes(header)).encrypt(transaction)

  const size = Math.ceil(ciphertext.length / t)
  const data = Array.from({ length: t }, (_, i) => {
    const shard = new Uint8Array(size)
    shard.set(ciphertext.subarray(i * size, (i + 1) * size))
    return shard
  })
  const shards = [...data, ...combine(encodingMatrix(t, n).slice(t), data)]

  // One polynomial of degree t - 1 per key byte: the byte is its value at 0, share i its value at i + 1.
  const coefficients = Array.from(key, byte => [byte, ...crypto.getRandomValues(new Uint8Array(t - 1))])
  const prefix = new Uint8Array(17)
  prefix[0] = 1
  prefix.set(nonce, 1)
  new DataView(prefix.buffer).setUint32(13, ciphertext.length, true)
  const signature = toBase58(headerSignature)

  return receivers.map((receiver, index) => ({
    header, n, t, index, receiver,
    fragment: toBase64(Uint8Array.of(...prefix, ...shards[index]!)),
    keyShare: toBase64(Uint8Array.of(1, index + 1, ...coefficients.map(c => c.reduceRight((value, coefficient) => shamir(value, index + 1) ^ coefficient, 0)))),
    headerSignature: signature,
  }))
}

/** Rebuilds the signed transaction from any t fragments of one seal, the way the node does. */
export function open(fragments: FragmentSubmission[]): Uint8Array {
  const { header, n, t } = fragments[0]!
  const chosen = fragments.slice(0, t)
  if (chosen.length < t || new Set(chosen.map(f => f.index)).size < t) throw invalid(`Need ${t} distinct fragments.`)
  const envelopes = chosen.map(f => fromBase64(f.fragment))
  const first = envelopes[0]!
  const length = new DataView(first.buffer, first.byteOffset).getUint32(13, true)

  const rows = encodingMatrix(t, n)
  const data = combine(invert(chosen.map(f => rows[f.index]!)), envelopes.map(e => e.subarray(17)))
  const ciphertext = Uint8Array.of(...data.flatMap(shard => [...shard])).subarray(0, length)

  // Lagrange interpolation at 0 over the shares' x coordinates.
  const key = new Uint8Array(32)
  chosen.forEach((fragment, i) => {
    const x = fragment.index + 1
    const basis = chosen.reduce((product, other, j) => {
      const xj = other.index + 1
      return i === j ? product : shamir(product, shamir(xj, inverse(shamir, x ^ xj)))
    }, 1)
    fromBase64(fragment.keyShare).subarray(2).forEach((value, byte) => { key[byte]! ^= shamir(value, basis) })
  })

  try {
    return chacha20poly1305(key, first.subarray(1, 13), headerBytes(header)).decrypt(ciphertext)
  } catch {
    throw invalid('The fragments do not open: ciphertext authentication failed.')
  }
}
