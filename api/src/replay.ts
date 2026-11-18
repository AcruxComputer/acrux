// Mainnet replay: the slots `acrux replay --json` recorded (scripts/replay-load.mjs loads them) as
// one range with its totals, and each slot's record whole.
import type { PrismaClient } from '@prisma/client'
import { ReplaySandwich, type ReplayRange } from '@acrux/contract'

export function emptyReplayRange(): ReplayRange {
  return { fromSlot: null, toSlot: null, slotsReplayed: 0, transactions: 0, acruxSlots: 0, sandwichesFound: 0, sandwichesPriced: 0, lamportsTaken: '0', lamportsKept: '0', readAt: null, sandwiches: [] }
}

export async function readReplayRange(db: PrismaClient): Promise<ReplayRange> {
  const rows = await db.replaySlot.findMany({ select: { slot: true, transactions: true, acruxSlots: true, sandwiches: true, readAt: true }, orderBy: { slot: 'asc' } })
  if (rows.length === 0) return emptyReplayRange()
  const sandwiches = rows.flatMap(row => (row.sandwiches as unknown as ReplaySandwich[]).map(s => ({ ...s, slot: Number(row.slot) })))
  const sol = sandwiches.filter(s => s.paid === 'SOL')
  // One front and back leg can sandwich several victims: each pair's take counts once.
  const takes = new Map(sol.map(s => [`${s.slot}:${s.front}`, BigInt(s.take)]))
  return {
    fromSlot: Number(rows[0]!.slot),
    toSlot: Number(rows.at(-1)!.slot),
    slotsReplayed: rows.length,
    transactions: rows.reduce((n, r) => n + r.transactions, 0),
    acruxSlots: rows.reduce((n, r) => n + r.acruxSlots, 0),
    sandwichesFound: sandwiches.length,
    sandwichesPriced: sandwiches.filter(s => s.repricing).length,
    lamportsTaken: [...takes.values()].reduce((n, t) => n + t, 0n).toString(),
    lamportsKept: sol.reduce((n, s) => n + BigInt(s.repricing?.keptPaid ?? '0'), 0n).toString(),
    readAt: rows.reduce((latest, r) => (r.readAt > latest ? r.readAt : latest), rows[0]!.readAt).toISOString(),
    sandwiches: sandwiches.reverse().slice(0, 500),
  }
}
