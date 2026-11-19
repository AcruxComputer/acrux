import { createHash } from 'node:crypto'
import BN from 'bn.js'
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { z } from 'zod'

const integer = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER)
const base64 = z.string().regex(/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/)
const decimal = z.string().regex(/^(0|[1-9]\d*)$/).refine(value => BigInt(value) <= 18_446_744_073_709_551_615n)
const wireProof = z.object({
  consensusSlot: integer.max(Math.floor((Number.MAX_SAFE_INTEGER - 3) / 4)),
  chainId: z.string().min(1),
  epochSlots: integer.positive().refine(value => value % 4 === 0),
  feeBurnFromSlot: integer.nullable().optional(),
  admissionFromSlot: integer.nullable().optional(),
  nativeMint: z.string().nullable().optional(),
  hash: z.string().regex(/^[0-9a-f]{64}$/), outboxRoot: z.string().regex(/^[0-9a-f]{64}$/),
  preimage: base64, outbox: z.array(base64),
  validators: z.array(z.string()).min(1).max(32), stakes: z.array(decimal).min(1).max(32),
  votes: z.array(z.object({ validator: integer.max(31), kind: z.enum(['notar', 'finalize']), signature: base64 })),
})
export type Entry = { kind: 'set'; epoch: number; validators: { key: PublicKey; stake: BN }[] } |
  { kind: 'withdrawal'; sequence: number; mint: PublicKey; recipient: PublicKey; amount: BN } |
  { kind: 'feeBurn'; epoch: number; amount: BN }
export type Proof = Omit<z.infer<typeof wireProof>, 'preimage' | 'outbox' | 'validators'> & { preimage: Buffer; outbox: Buffer[]; validators: PublicKey[]; entries: Entry[] }
const sha256 = (...parts: Buffer[]) => createHash('sha256').update(Buffer.concat(parts)).digest()
export const u64 = (value: number) => { const bytes = Buffer.alloc(8); bytes.writeBigUInt64LE(BigInt(value)); return bytes }
export const exact = (value: BN | bigint | number, label: string): number => {
  const n = Number(value.toString())
  if (!Number.isSafeInteger(n) || n < 0) throw new Error(`Invalid ${label}.`)
  return n
}

function decodeEntry(bytes: Buffer): Entry {
  if (bytes.length === 20 && bytes.readUInt32LE(0) === 2) return {
    kind: 'feeBurn', epoch: exact(bytes.readBigUInt64LE(4), 'fee burn epoch'), amount: new BN(bytes.readBigUInt64LE(12).toString()),
  }
  if (bytes.length === 84 && bytes.readUInt32LE(0) === 1) return {
    kind: 'withdrawal', sequence: exact(bytes.readBigUInt64LE(4), 'withdrawal sequence'),
    mint: new PublicKey(bytes.subarray(12, 44)), recipient: new PublicKey(bytes.subarray(44, 76)), amount: new BN(bytes.readBigUInt64LE(76).toString()),
  }
  if (bytes.length < 20 || bytes.readUInt32LE(0) !== 0) throw new Error('Invalid outbox entry.')
  const count = exact(bytes.readBigUInt64LE(12), 'validator count')
  if (count < 1 || count > 32 || bytes.length !== 20 + count * 40) throw new Error('Invalid outbox validator set.')
  return { kind: 'set', epoch: exact(bytes.readBigUInt64LE(4), 'outbox epoch'), validators: Array.from({ length: count }, (_, i) => ({
    key: new PublicKey(bytes.subarray(20 + i * 40, 52 + i * 40)), stake: new BN(bytes.readBigUInt64LE(52 + i * 40).toString()),
  })) }
}

export function readProof(raw: unknown, epochSlots: number, chainId: Buffer): Proof {
  const wire = wireProof.parse(raw)
  if (!sha256(Buffer.from(wire.chainId)).equals(chainId)) throw new Error('Gateway proof belongs to another chain.')
  const preimage = Buffer.from(wire.preimage, 'base64'), outbox = wire.outbox.map(leaf => Buffer.from(leaf, 'base64'))
  const validators = wire.validators.map(key => new PublicKey(key)), entries = outbox.map(decodeEntry)
  if (wire.epochSlots !== epochSlots || validators.length !== wire.stakes.length || preimage.length < 8 || preimage.readBigUInt64LE(0) !== BigInt(wire.consensusSlot)) throw new Error('Gateway proof does not match the bridge epoch or cblock.')
  if (sha256(preimage, Buffer.from(wire.outboxRoot, 'hex')).toString('hex') !== wire.hash) throw new Error('Gateway cblock hash is invalid.')
  let level = outbox.map(leaf => sha256(leaf))
  while (level.length > 1) level = Array.from({ length: Math.ceil(level.length / 2) }, (_, i) => i * 2 + 1 < level.length ? sha256(level[i * 2], level[i * 2 + 1]) : level[i * 2])
  if ((level[0] ?? Buffer.alloc(32)).toString('hex') !== wire.outboxRoot) throw new Error('Gateway outbox root is invalid.')
  const next = entries.filter(entry => entry.kind === 'set')
  if (wire.admissionFromSlot != null && (wire.admissionFromSlot % epochSlots !== 0 || epochSlots % 8 !== 0)) throw new Error('Gateway admission rule is invalid.')
  const firstSlot = wire.consensusSlot * 4
  const beforeCutoff = wire.admissionFromSlot != null && firstSlot >= wire.admissionFromSlot && firstSlot % epochSlots < epochSlots / 2
  if (next.length !== (beforeCutoff ? 0 : 1) || next.some(entry => entry.epoch !== Math.floor(firstSlot / epochSlots) + 1)) throw new Error('Gateway outbox epoch is invalid.')
  const burns = entries.filter(entry => entry.kind === 'feeBurn')
  if (burns.length > 1 || burns.some(entry => entry.epoch !== Math.floor(wire.consensusSlot * 4 / epochSlots) || (wire.consensusSlot + 1) * 4 % epochSlots !== 0)) throw new Error('Gateway fee burn is not the epoch-ending cblock.')
  if (wire.feeBurnFromSlot != null && (wire.feeBurnFromSlot % epochSlots !== 0 || !wire.nativeMint)) throw new Error('Gateway fee burn rule is invalid.')
  if (wire.nativeMint != null) new PublicKey(wire.nativeMint)
  if (burns.length && wire.feeBurnFromSlot != null && wire.consensusSlot * 4 < wire.feeBurnFromSlot) throw new Error('Gateway fee burn precedes its activation.')
  if (wire.votes.some(vote => vote.validator >= validators.length || Buffer.from(vote.signature, 'base64').length !== 64)) throw new Error('Gateway vote is invalid.')
  return { ...wire, preimage, outbox, validators, entries }
}

export function outboxProof(leaves: Buffer[], index: number) {
  let level = leaves.map(leaf => sha256(leaf))
  const proof: { left: boolean; hash: number[] }[] = []
  while (level.length > 1) {
    const sibling = index ^ 1
    if (sibling < level.length) proof.push({ left: sibling < index, hash: [...level[sibling]] })
    level = Array.from({ length: Math.ceil(level.length / 2) }, (_, i) => i * 2 + 1 < level.length ? sha256(level[i * 2], level[i * 2 + 1]) : level[i * 2])
    index >>= 1
  }
  return proof
}

/** Bincode Vote and precompile offsets match chain/scripts/bridge-relay.mjs. */
export function attestInstruction(proof: Proof, votes: Proof['votes']) {
  const head = Buffer.alloc(2 + votes.length * 14), bodies: Buffer[] = []
  head[0] = votes.length
  let at = head.length
  for (const [index, vote] of votes.entries()) {
    const kind = Buffer.alloc(4); kind.writeUInt32LE(vote.kind === 'finalize' ? 4 : 0)
    const message = Buffer.concat([sha256(Buffer.from(proof.chainId)), u64(vote.validator), u64(proof.consensusSlot), Buffer.from([1]), Buffer.from(proof.hash, 'hex'), kind])
    for (const [offset, value] of [[0, at + 32], [2, 0xffff], [4, at], [6, 0xffff], [8, at + 96], [10, message.length], [12, 0xffff]]) head.writeUInt16LE(value, 2 + index * 14 + offset)
    bodies.push(proof.validators[vote.validator].toBuffer(), Buffer.from(vote.signature, 'base64'), message)
    at += 96 + message.length
  }
  return new TransactionInstruction({ programId: new PublicKey('Ed25519SigVerify111111111111111111111111111'), keys: [], data: Buffer.concat([head, ...bodies]) })
}
