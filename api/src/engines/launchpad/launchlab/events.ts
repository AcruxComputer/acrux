// Reading trades back from confirmed transactions: LaunchLab's TradeEvent and PoolCreateEvent
// (emitted as a self-CPI, so they sit in inner instructions) and CPMM's SwapEvent (emitted as a
// "Program data:" log). For indexers and for checking a fill against its quote.

import { PublicKey, type VersionedTransactionResponse } from '@solana/web3.js'
import { CPMM_PROGRAM_ID, LAUNCHLAB_PROGRAM_ID } from './addresses.js'
import { decodeLaunchlabEvent } from './codec.js'
import { fromBase58, fromBase64 } from '../../core/encoding.js'

/** Every field is on the event except the trader, mints and platform: read those from the instruction or the pool. */
export type TradeEvent = {
  poolState: PublicKey; totalBaseSell: bigint; virtualBase: bigint; virtualQuote: bigint
  realBaseBefore: bigint; realQuoteBefore: bigint; realBaseAfter: bigint; realQuoteAfter: bigint
  /** Gross quote paid on a buy, base sold on a sell. */
  amountIn: bigint
  /** Base received on a buy, quote received after fees on a sell. */
  amountOut: bigint
  protocolFee: bigint; platformFee: bigint; creatorFee: bigint; shareFee: bigint
  tradeDirection: 'buy' | 'sell'
  /** 'migrate' after the buy that sold the curve out. */
  poolStatus: 'fund' | 'migrate' | 'trade'
  exactIn: boolean
}

export type PoolCreateEvent = {
  poolState: PublicKey; creator: PublicKey; config: PublicKey
  baseMintParam: { decimals: number; name: string; symbol: string; uri: string }
  curveParam: { constant?: { data: { supply: bigint; totalBaseSell: bigint; totalQuoteFundRaising: bigint; migrateType: number } } }
  vestingParam: { totalLockedAmount: bigint; cliffPeriod: bigint; unlockPeriod: bigint }
  ammFeeOn: 'quoteToken' | 'bothToken'
}

export type LaunchlabEvent =
  | { name: 'TradeEvent'; data: TradeEvent; instruction: number }
  | { name: 'PoolCreateEvent'; data: PoolCreateEvent; instruction: number }
  | { name: string; data: unknown; instruction: number }

/** Anchor's emit_cpi tag, ahead of the event's own discriminator. */
const EVENT_CPI_TAG = Buffer.from([228, 69, 165, 46, 81, 203, 154, 29])

/**
 * LaunchLab events in a successful transaction, with the index of the top-level instruction that
 * produced each (direct calls and calls through another program alike). Only LaunchLab can sign
 * its event-authority self-CPI, so an event inside a successful transaction is LaunchLab's.
 */
export function launchlabEvents(tx: VersionedTransactionResponse): LaunchlabEvent[] {
  if (tx.meta?.err) return []
  const keys = tx.transaction.message.getAccountKeys({ accountKeysFromLookups: tx.meta?.loadedAddresses })
  const out: LaunchlabEvent[] = []
  for (const group of tx.meta?.innerInstructions ?? []) {
    for (const ix of group.instructions) {
      if (!keys.get(ix.programIdIndex)?.equals(LAUNCHLAB_PROGRAM_ID)) continue
      const data = Buffer.from(fromBase58(ix.data))
      if (!data.subarray(0, 8).equals(EVENT_CPI_TAG)) continue
      const event = decodeLaunchlabEvent(data.subarray(8))
      if (event) out.push({ ...event, instruction: group.index } as LaunchlabEvent)
    }
  }
  return out
}

export const tradeEvents = (tx: VersionedTransactionResponse) =>
  launchlabEvents(tx).filter((e): e is Extract<LaunchlabEvent, { name: 'TradeEvent' }> => e.name === 'TradeEvent').map((e) => e.data as TradeEvent)

export type CpmmSwapEvent = {
  pool: PublicKey; inputVaultBefore: bigint; outputVaultBefore: bigint; inputAmount: bigint; outputAmount: bigint
  inputTransferFee: bigint; outputTransferFee: bigint; baseInput: boolean; inputMint: PublicKey; outputMint: PublicKey
  tradeFee: bigint; creatorFee: bigint; creatorFeeOnInput: boolean
}

const SWAP_EVENT = Buffer.from([64, 198, 205, 232, 38, 8, 113, 226])

/**
 * CPMM SwapEvents from a transaction's logs. A log line counts only while CPMM is the program
 * running, tracked through the invoke/success stack, so another program cannot forge one.
 */
export function cpmmSwapEvents(logs: readonly string[]): CpmmSwapEvent[] {
  const stack: string[] = [], out: CpmmSwapEvent[] = []
  const cpmm = CPMM_PROGRAM_ID.toBase58()
  for (const line of logs) {
    const invoke = line.match(/^Program (\S+) invoke \[(\d+)\]$/)
    if (invoke) { stack.length = Number(invoke[2]) - 1; stack.push(invoke[1]!); continue }
    if (/^Program \S+ (success|failed)/.test(line)) { stack.pop(); continue }
    if (stack.at(-1) !== cpmm || !line.startsWith('Program data: ')) continue
    const d = Buffer.from(fromBase64(line.slice(14)))
    if (d.length < 170 || !d.subarray(0, 8).equals(SWAP_EVENT)) continue
    const u = (o: number) => d.readBigUInt64LE(o), k = (o: number) => new PublicKey(d.subarray(o, o + 32))
    out.push({
      pool: k(8), inputVaultBefore: u(40), outputVaultBefore: u(48), inputAmount: u(56), outputAmount: u(64), inputTransferFee: u(72), outputTransferFee: u(80),
      baseInput: d[88] === 1, inputMint: k(89), outputMint: k(121), tradeFee: u(153), creatorFee: u(161), creatorFeeOnInput: d[169] === 1,
    })
  }
  return out
}
