import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { ed25519 } from '@noble/curves/ed25519.js'
import { fromBase58, fromBase64, toBase64, FragmentSubmission } from '@acrux/contract'
import { seal, open, deriveHeader, headerBytes } from '../dist/index.js'

const vector = name => JSON.parse(readFileSync(new URL(`./vectors/${name}.json`, import.meta.url), 'utf8'))
const subsets = (items, size) => size === 0 ? [[]] : items.flatMap((item, i) => subsets(items.slice(i + 1), size - 1).map(rest => [item, ...rest]))
const shard = fragment => fromBase64(fragment.fragment).subarray(17)

for (const [n, t] of [[2, 2], [3, 2], [3, 3]]) {
  const rust = vector(`rust-${t}-of-${n}`)
  const bytes = fromBase64(rust.plain.transaction)

  test(`${t} of ${n}: the header derived from the bytes is the node's, and its signed bytes verify`, () => {
    const header = deriveHeader(bytes, rust.plain.header.targetSlot)
    assert.deepEqual(header, rust.plain.header)
    assert.ok(ed25519.verify(fromBase58(rust.fragments[0].headerSignature), headerBytes(header), fromBase58(header.feePayer)))
  })

  test(`${t} of ${n}: every t of the Rust encoder's fragments open in TypeScript`, () => {
    for (const chosen of subsets(rust.fragments, t)) {
      assert.deepEqual(open(chosen), bytes)
      assert.deepEqual(open(chosen.toReversed()), bytes)
    }
  })

  test(`${t} of ${n}: TypeScript fragments open from every t, and match the contract`, () => {
    const header = rust.plain.header
    const sealed = seal(bytes, header, fromBase58(rust.fragments[0].headerSignature), t, rust.plain.receivers)
    assert.equal(sealed.length, n)
    for (const fragment of sealed) {
      FragmentSubmission.parse(fragment)
      const rustFragment = rust.fragments[fragment.index]
      assert.equal(fragment.fragment.length, rustFragment.fragment.length)
      assert.equal(fragment.keyShare.length, rustFragment.keyShare.length)
      assert.equal(fromBase64(fragment.keyShare)[1], fragment.index + 1)
    }
    for (const chosen of subsets(sealed, t)) assert.deepEqual(open(chosen), bytes)
    if (t > 1) assert.throws(() => open(sealed.slice(0, t - 1)))
    assert.throws(() => open(sealed.slice(0, t).map(f => ({ ...f, header: { ...header, targetSlot: header.targetSlot + 1 } }))))
  })
}

test('the parity shard is the reed-solomon-erasure galois_8 parity of the Rust data shards', () => {
  // For 2 data shards and 1 parity shard the crate's matrix row is [3, 2] over 0x11d.
  const rust = vector('rust-2-of-3')
  const [d0, d1, parity] = rust.fragments.map(shard)
  const mul = (a, b) => { let p = 0; for (; b; b >>= 1) { if (b & 1) p ^= a; a = a & 0x80 ? ((a << 1) ^ 0x1d) & 0xff : a << 1 } return p }
  assert.deepEqual(parity, d0.map((byte, i) => mul(3, byte) ^ mul(2, d1[i])))
})

test('a TypeScript seal carries the same parity bytes the Rust encoder would write', () => {
  const rust = vector('rust-2-of-3')
  const bytes = fromBase64(rust.plain.transaction)
  const sealed = seal(bytes, rust.plain.header, fromBase58(rust.fragments[0].headerSignature), 2, [1, 2, 3])
  const [d0, d1, parity] = sealed.map(shard)
  const mul = (a, b) => { let p = 0; for (; b; b >>= 1) { if (b & 1) p ^= a; a = a & 0x80 ? ((a << 1) ^ 0x1d) & 0xff : a << 1 } return p }
  assert.deepEqual(parity, d0.map((byte, i) => mul(3, byte) ^ mul(2, d1[i])))
  assert.equal(toBase64(fromBase64(sealed[0].fragment).subarray(0, 1)), 'AQ==')
})
