# curve

LaunchLab's bonding curve and Raydium CPMM in integer math, rounded the way the programs round:
launch reserves, buys and sells in both directions with the whole fee stack, the capped last buy,
the buy that finishes a curve, price and impact, CPMM swaps with the creator fee on either side,
creator-fee payouts and Fee Key accrual. Pure `bigint`, no imports beyond the folder, so the same
quote runs in a browser, an API and a keeper. A float version of the curve's shape serves charts.

## Modules

| File | What it does | Needs |
|---|---|---|
| `math.ts` | `RATE_DENOMINATOR`, `ceilDiv`, `grossUp`, `minimumOut` / `maximumIn` (slippage), `isqrt`, `ratioText` / `priceText` (exact decimal text), `toUnits` / `formatUnits` | nothing |
| `launchlab.ts` | `initialReserves`, `launchState`, `buyExactIn`, `buyExactOut`, `sellExactIn`, `sellExactOut`, `splitFees`, `completionQuote`, `spotPrice`, `priceImpactBps`, `progressBps` | `math.ts` |
| `cpmm.ts` | `swapBaseInput`, `swapBaseOutput`, `creatorFeeOnInput`, `cpmmReserves`, `creatorFeePayout`, `feeKeyAccrued`, `lpToTokens` | `math.ts` |
| `display.ts` | `curveShape(params)`: the curve in floating point and whole tokens (price, raised, sold, points for a chart). Shape only | nothing |

`../launchlab/accounts.ts` turns a decoded pool into these inputs: `curveState(pool)` and
`curveFeeRates(globalConfig, platformConfig, shareFeeRate)`; `../launchlab/cpmm.ts` gives `cpmmRates`.

## Assembly

```ts
import { buyExactIn, sellExactOut, completionQuote, priceImpactBps, type CurveState } from './engines/launchpad/curve/launchlab'
import { minimumOut, maximumIn, priceText } from './engines/launchpad/curve/math'

const rates = { protocol: 2_500n, platform: 10_000n, creator: 5_000n }    // from the pool's configs
const q = buyExactIn(state, 1_000_000_000n, rates)
q.base                         // tokens out
q.fees                         // { total, protocol, platform, creator, share }
q.capped                       // the buy reached the end of the curve and was cut
minimumOut(q.base, 100)        // the trade's minimum_amount_out at 1%
priceImpactBps(state, q)
priceText(q.after.virtualQuote + q.after.realQuote, q.after.virtualBase - q.after.realBase, 6, 9)

const out = sellExactOut(state, quoteWanted, rates)
maximumIn(out.base, 100)       // maximum_amount_in

const finish = completionQuote(state, rates)   // the input that sells the curve out
```

```ts
import { swapBaseInput, creatorFeeOnInput, cpmmReserves, feeKeyAccrued, lpToTokens } from './engines/launchpad/curve/cpmm'

const { reserve0, reserve1 } = cpmmReserves(vault0, vault1, pool)
const onInput = creatorFeeOnInput(pool.creatorFeeOn, inputIsToken0)
const s = swapBaseInput(amountIn, inputIsToken0 ? reserve0 : reserve1, inputIsToken0 ? reserve1 : reserve0, rates, onInput)
const lp = feeKeyAccrued(lock, pool.lpSupply, reserve0, reserve1)
const harvest = lpToTokens(lp, pool.lpSupply, reserve0, reserve1)
```

## Behaviour worth knowing

- LaunchLab charges its fees in the quote token: on a buy's input, on a sell's output. It rounds the
  combined fee (protocol + platform + creator + share) up once, floors the platform, creator and
  share parts, and leaves the remainder to the protocol. Exact-out amounts gross up with a ceiling.
- `realBase` counts tokens sold off the curve, not the base vault's balance; the reserves the
  product runs on are `virtualBase − realBase` and `virtualQuote + realQuote`.
- `initialReserves` is Raydium's `getInitParam`, integer for integer: the GlobalConfig's migrate fee
  comes off the raise and the vesting lock off the base that seeds the pool. Supply 10^15 with
  793.1·10^12 on the curve and an 85·10^9 raise gives 1,073,025,605,596,382 / 30,000,852,951, the
  reserves mainnet pools launched with those arguments carry.
- A buy larger than the base left is cut to that base and charged only what it costs; that buy
  moves the pool to status 1. The curve completes when its base is sold out, not when `realQuote`
  reaches the raise: rounded buys and sells move the cost off the nominal raise.
- `completionQuote` adds one quote unit before the gross-up so the rounded fee never leaves the
  finishing buy a unit short; the buy then takes the capped branch and pays the exact cost.
- `sellExactOut` refuses a sale needing more quote than the curve holds or more tokens than it sold.
- CPMM rounds a creator fee on the input together with the trade fee (one ceiling, creator part
  floored); on the output it rounds the creator fee up alone. Protocol and fund cuts are floored
  shares of the trade fee; the LP keeps the rest. Raydium's TypeScript SDK ceils the two input fees
  separately on exact-in; the program, and these functions, do not.
- Tradable CPMM reserves are vault balances less the protocol, fund and creator fees still in the
  pool. Pass a creator rate of zero for a pool whose creator fee is off.
- `feeKeyAccrued` is `locked − locked·√lastK·lpSupply / (√k·lastLp)` plus what is unclaimed, holding
  back one LP unit at each integer root so it never asks for more than LP-Lock pays. A harvest
  whose `lpToTokens` floors either side to zero fails inside CPMM.
- A base mint with a Token-2022 transfer fee withholds part of each base transfer; apply it to the
  base amount before (sells) or after (buys) the curve quote.
- The exact-in quotes reproduce mainnet LaunchLab TradeEvents (amounts, each fee, reserves after)
  and CPMM SwapEvents with the creator fee on either side; every path, exact-out included, matches
  Raydium's SDK wherever SDK and program agree.
