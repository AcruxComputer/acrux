// The LaunchLab curve in floating point, in whole tokens, for charts and sliders. The shape only:
// quotes and anything that settles use launchlab.ts. With N supply, S sold on the curve, L
// locked, M = N − S − L seeding the pool, a raise T and a migrate fee F, the program's virtual
// reserves are a = (T−F)·S² / (M·d) and b = T² / d with d = (T−F)·S/M − T.

export type CurveShapeParams = { supply: number; totalBaseSell: number; raise: number; locked?: number; migrateFee?: number }

export function curveShape(p: CurveShapeParams) {
  const migrate = p.supply - p.totalBaseSell - (p.locked ?? 0)
  const raised = p.raise - (p.migrateFee ?? 0)
  const d = (raised * p.totalBaseSell) / migrate - p.raise
  const a = (raised * p.totalBaseSell * p.totalBaseSell) / migrate / d
  const b = (p.raise * p.raise) / d
  const S = p.totalBaseSell

  /** Spot price (quote per token) once `sold` tokens have left the curve. */
  const priceAt = (sold: number) => (b * a) / (a - sold) ** 2
  /** Quote raised by the time `sold` tokens have left, before fees. */
  const raisedAt = (sold: number) => (b * sold) / (a - sold)
  /** Tokens sold by the time `quote` has been raised, before fees. */
  const soldAt = (quote: number) => (a * quote) / (b + quote)
  /** Tokens bought by `quoteIn` after fees, starting from `sold`. */
  const tokensFor = (quoteIn: number, sold: number) => {
    const x = a - sold, y = b + raisedAt(sold)
    return Math.min(x - (x * y) / (y + quoteIn), S - sold)
  }
  /** Sold amount at which the market cap (price × supply) reaches `mcap`. */
  const soldAtMcap = (mcap: number) => a - Math.sqrt((b * a * p.supply) / mcap)
  /** `count` evenly spaced points from launch to the last token, for a price chart. */
  const points = (count: number) =>
    Array.from({ length: count }, (_, i) => {
      const sold = (S * i) / (count - 1)
      return { sold, price: priceAt(sold), raised: raisedAt(sold) }
    })

  return { a, b, priceAt, raisedAt, soldAt, tokensFor, soldAtMcap, points, startPrice: priceAt(0), endPrice: priceAt(S), climb: (a / (a - S)) ** 2 }
}
