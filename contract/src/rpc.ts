import { z } from 'zod'
import { ApiError, Balance, Count, Counters, DemoPool, Facts, FragmentSubmission, NetSnapshot, PlainSubmission, Quote, QuoteRequest, Slot, Submission, Transaction, TxId, Window, Address } from './schemas.js'
import { LabReplay } from './events.js'
import { Signature, U64, PageQuery } from './primitives.js'
const empty = z.strictObject({})
export const rpcMethods = {
  /** Bridge epochs from the native cut-over. Times are Unix milliseconds; cursor is an exclusive epoch. */
  getFeeBurns: { params: PageQuery, result: z.strictObject({
    program: Address.nullable(), mint: Address.nullable(), nativeFromSlot: Count.nullable(), epochSlots: Count.min(1),
    items: z.array(z.strictObject({
      epoch: Count, firstSlot: Count, lastSlot: Count, startedAt: Count.nullable(), endedAt: Count.nullable(), closed: z.boolean(),
      baseFeeBurnedLamports: U64.nullable(), feeBurnAmountBaseUnits: U64.nullable(), remainderLamports: U64.nullable(),
    })), nextCursor: z.string().regex(/^[0-9]+$/).nullable(), feeBurnedBaseUnits: U64,
  }) },
  getSnapshot: { params: empty, result: NetSnapshot },
  getSlot: { params: z.strictObject({ slot: Count }), result: Slot },
  getRecentSlots: { params: z.strictObject({ limit: Count.min(1).max(64).default(64) }), result: z.array(Slot) },
  getCurrentWindow: { params: empty, result: Window },
  getSchedule: { params: z.strictObject({ limit: Count.min(1).max(64).default(8) }), result: z.array(Window) },
  getCounters: { params: empty, result: Counters },
  /** By the transaction's id, or by its Solana signature (the first signature of the signed bytes). */
  getTransaction: { params: z.union([z.strictObject({ id: TxId }), z.strictObject({ signature: Signature })]), result: Transaction },
  /** Implemented by the Rust node; the unchanged Round 1 fixture gateway has no real blockhash. */
  getLatestBlockhash: { params: empty, result: z.strictObject({ blockhash: Address, slot: Count, startedSlot: Count }) },
  /** Rolling measurements of the live net (the last 10 minutes). */
  getFacts: { params: empty, result: Facts },
  getBalance: { params: z.strictObject({ address: Address }), result: Balance },
  getDemoPool: { params: empty, result: DemoPool },
  getQuote: { params: QuoteRequest, result: Quote },
  submitTransaction: { params: PlainSubmission, result: Submission },
  submitFragment: { params: FragmentSubmission, result: Submission },
  getLabReplay: { params: z.strictObject({ runId: z.string().min(1) }), result: LabReplay },
} as const
export type RpcMethod = keyof typeof rpcMethods
export type RpcParams<M extends RpcMethod> = z.input<(typeof rpcMethods)[M]['params']>
export type RpcResult<M extends RpcMethod> = z.output<(typeof rpcMethods)[M]['result']>
export const RpcRequest = z.strictObject({ jsonrpc: z.literal('2.0'), id: z.union([z.string(), Count]), method: z.string(), params: z.unknown().optional() })
export const RpcFailure = z.strictObject({ jsonrpc: z.literal('2.0'), id: z.union([z.string(), Count, z.null()]), error: z.strictObject({ code: z.number().int(), message: z.string(), data: ApiError }) })
export type RpcFailure = z.infer<typeof RpcFailure>
