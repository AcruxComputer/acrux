import { Buyback, BuybackAction } from '@acrux/contract'
import type { PrismaClient } from '@prisma/client'

type Settings = { mint: string; wallet: string }

export function emptyBuyback({ mint, wallet }: Settings): Buyback {
  return {
    status: mint ? 'live' : 'waiting', mint: mint || null, wallet,
    totals: { claimedLamports: '0', swappedLamports: '0', acruxBought: '0', acruxBurned: '0' },
    rows: [],
  }
}

/** Read only confirmed product rows, never journal bytes or wallet balances. */
export async function readBuyback(db: PrismaClient, settings: Settings): Promise<Buyback> {
  const result = emptyBuyback(settings)
  if (!settings.mint) return result
  const where = { mint: settings.mint, wallet: settings.wallet }
  const [totals, rows] = await db.$transaction([
    db.buybackRow.groupBy({ by: ['action'], where, orderBy: { action: 'asc' }, _sum: { lamports: true, tokenAmount: true } }),
    db.buybackRow.findMany({ where, orderBy: [{ createdAt: 'desc' }, { id: 'desc' }], take: 50 }),
  ])
  for (const total of totals) {
    if (total.action === 'claim') result.totals.claimedLamports = total._sum?.lamports?.toFixed(0) ?? '0'
    if (total.action === 'swap') {
      result.totals.swappedLamports = total._sum?.lamports?.toFixed(0) ?? '0'
      result.totals.acruxBought = total._sum?.tokenAmount?.toFixed(0) ?? '0'
    }
    if (total.action === 'burn') result.totals.acruxBurned = total._sum?.tokenAmount?.toFixed(0) ?? '0'
  }
  result.rows = rows.map(row => ({
    id: row.id, cycleId: row.cycleId, action: BuybackAction.parse(row.action), signature: row.signature,
    lamports: row.lamports.toFixed(0), tokenAmount: row.tokenAmount.toFixed(0), createdAt: row.createdAt.toISOString(),
  }))
  return result
}
