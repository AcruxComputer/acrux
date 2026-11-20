import { z } from 'zod'
import { ApiError, Attestation, Certificate, Commitment, CommitmentArrival, Count, Cblock, Counters, Evidence, ExecutionSchedule, LabRun, MergedTransaction, NetSnapshot, ProposerIndex, Skip, Slice, Slot, Transaction, TxId, Window } from './schemas.js'

// One version per envelope. Null slot is reserved for stream/window control messages.
// atUs is relative to the named anchor, never an epoch timestamp. Slot.wallTime is UTC.
const common = { node: z.string().optional(), version: z.literal(1), sequence: Count, streamId: z.string(), slot: Count.nullable(), anchor: z.enum(['slot', 'window', 'consensus', 'stream']), atUs: Count }
const event = <T extends string, S extends z.ZodType>(type: T, data: S) => z.object({ ...common, type: z.literal(type), data })
export const ProtocolEvent = z.discriminatedUnion('type', [
  event('stream.snapshot', NetSnapshot),
  event('stream.resync', z.object({ reason: z.enum(['connected', 'cursor_expired', 'restarted', 'filter_changed']), snapshot: NetSnapshot })),
  event('stream.heartbeat', z.object({ latestSlot: Count, status: z.enum(['live', 'stalled', 'unavailable']) })),
  event('stream.error', ApiError),
  event('window.started', Window),
  event('slot.started', Slot),
  event('block.slice', z.object({ proposer: ProposerIndex, slice: Slice })),
  event('commitment.sent', Commitment),
  event('commitment.arrived', z.object({ proposer: ProposerIndex, hash: z.string(), arrival: CommitmentArrival })),
  event('attestation.sent', Attestation),
  event('attestation.arrived', z.object({ attestation: Attestation, validator: z.string(), arrivedUs: Count })),
  event('slot.agreed', z.object({ agreedSet: z.array(ProposerIndex) })),
  event('slot.skipped', Skip),
  event('block.decrypted', z.object({ proposer: ProposerIndex, transactionIds: z.array(TxId) })),
  event('slot.merged', z.object({ transactions: z.array(MergedTransaction) })),
  event('slot.scheduled', ExecutionSchedule),
  event('slot.executed', Slot),
  event('cblock.assembled', Cblock),
  event('cblock.vote', z.lazy(() => Vote)),
  event('cblock.certificate', Certificate),
  event('cblock.finalized', Cblock),
  event('transaction.updated', Transaction),
  event('net.counters', Counters),
  event('lab.updated', LabRun),
  event('lab.evidence', Evidence),
])
// Imported separately for readable event catalogue ordering.
import { Vote } from './schemas.js'
export type ProtocolEvent = z.infer<typeof ProtocolEvent>
export const SubscribeFilter = z.discriminatedUnion('kind', [
  z.strictObject({ kind: z.literal('all') }),
  z.strictObject({ kind: z.literal('slots'), from: Count, to: Count }).refine(v => v.from <= v.to, 'from must not exceed to'),
  z.strictObject({ kind: z.literal('transaction'), id: TxId }),
])
export type SubscribeFilter = z.infer<typeof SubscribeFilter>
export const StreamCommand = z.discriminatedUnion('type', [
  z.strictObject({ version: z.literal(1), type: z.literal('subscribe'), filter: SubscribeFilter, afterSequence: Count.optional(), streamId: z.string().optional() }),
  z.strictObject({ version: z.literal(1), type: z.literal('unsubscribe') }),
])
export const LabReplay = z.object({ run: LabRun, events: z.array(ProtocolEvent) })
export type LabReplay = z.infer<typeof LabReplay>
export function matchesFilter(event: ProtocolEvent, filter: SubscribeFilter): boolean {
  if (event.type.startsWith('stream.')) return true
  if (filter.kind === 'all') return true
  if (filter.kind === 'slots') return event.slot !== null && event.slot >= filter.from && event.slot <= filter.to
  if (event.type === 'transaction.updated') return event.data.header.id === filter.id
  if (event.type === 'slot.merged') return event.data.transactions.some(tx => tx.id === filter.id)
  if (event.type === 'slot.executed' || event.type === 'slot.started') return event.data.mergedOrder.some(tx => tx.id === filter.id)
  if (event.type === 'block.decrypted') return event.data.transactionIds.includes(filter.id)
  if (event.type === 'lab.evidence') return event.data.txId === filter.id
  return false
}