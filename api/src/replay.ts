// Mainnet replay: the slots `acrux replay --json` recorded (scripts/replay-load.mjs loads them) as
// one range with its totals, and each slot's record whole.
import type { PrismaClient } from '@prisma/client'
import type { ReplayRange, ReplaySandwich } from '@acrux/contract'

export function emptyReplayRange(): ReplayRange {
  return { fromSlot: null, toSlot: null, slotsReplayed: 0, transactions: 0, acruxSlots: 0, sandwichCount: 0, sandwichesFound: 0, sandwichesPriced: 0, lamportsTaken: '0', lamportsKept: '0', readAt: null, sandwiches: [] }
}

/**
 * Totals over the replayed slots from `from` to `to` (every one when unset), counted in the
 * database; an epoch is 432,000 rows.
 */
export async function readReplayRange(db: PrismaClient, range: { from?: number; to?: number } = {}): Promise<ReplayRange> {
  const [from, to] = [BigInt(range.from ?? 0), BigInt(range.to ?? Number.MAX_SAFE_INTEGER)]
  const totals = await db.replaySlot.aggregate({ where: { slot: { gte: from, lte: to } }, _count: true, _sum: { transactions: true, acruxSlots: true }, _min: { slot: true }, _max: { slot: true, readAt: true } })
  if (totals._count === 0) return emptyReplayRange()
  const rows = await db.$queryRaw<{ slot: bigint; sandwiches: ReplaySandwich[] }[]>`SELECT slot, sandwiches FROM replay_slots WHERE slot BETWEEN ${from} AND ${to} AND jsonb_array_length(sandwiches) > 0 ORDER BY slot`
  const sandwiches = rows.flatMap(row => row.sandwiches.map(s => ({ ...s, slot: Number(row.slot) })))
  const sol = sandwiches.filter(s => s.paid === 'SOL')
  // One front and back leg can sandwich several victims: each pair's take counts once.
  const takes = new Map(sol.map(s => [`${s.slot}:${s.front}:${s.back}`, BigInt(s.take)]))
  return {
    fromSlot: Number(totals._min.slot),
    toSlot: Number(totals._max.slot),
    slotsReplayed: totals._count,
    transactions: totals._sum.transactions ?? 0,
    acruxSlots: totals._sum.acruxSlots ?? 0,
    sandwichCount: new Set(sandwiches.map(s => `${s.slot}:${s.front}:${s.back}`)).size,
    sandwichesFound: sandwiches.length,
    sandwichesPriced: sandwiches.filter(s => s.repricing).length,
    lamportsTaken: [...takes.values()].reduce((n, t) => n + t, 0n).toString(),
    lamportsKept: sol.reduce((n, s) => n + BigInt(s.repricing?.keptPaid ?? '0'), 0n).toString(),
    readAt: totals._max.readAt!.toISOString(),
    sandwiches: sandwiches.reverse().slice(0, 500),
  }
}
