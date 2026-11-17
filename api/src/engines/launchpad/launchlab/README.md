# launchlab

Raydium LaunchLab without the Raydium SDK. Launch a coin on any GlobalConfig (any quote mint,
either token program), trade its curve, run a platform and keep it at a spec, claim platform and
creator fees, follow the coin through graduation into CPMM, collect its CPMM creator fee and
harvest its Fee Key. Builders return `TransactionInstruction`s; compiling, signing and sending
belong to the `tx` engine. Quotes come from `../curve`.

`../idls/launchlab.json` is LaunchLab's on-chain IDL account `E6wT2uNeoWUvDrwdch1R8ETsyewR1ZM4WAwsa5hLJK5Z`
(`raydium_launchpad` 0.2.0, 28 instructions), copied byte for byte. CPMM and LP-Lock calls are hand
encoded: CPMM has no on-chain IDL and the public one predates the calls used here.

## Modules

| File | What it does | Needs |
|---|---|---|
| `addresses.ts` | Program ids, Raydium's fixed accounts (`RAYDIUM.migrationCrank`, `createPoolFee`, `lookupTable`), every PDA: GlobalConfig, platform, pool and vaults, fee vaults and their authorities, allow config, curve rule, the CPMM pool set, LaunchLab's CPMM permission, support mints, creator fee share, LP-Lock | `@solana/web3.js`, `@solana/spl-token` |
| `codec.ts` | The IDL as encoder (`launchlabInstruction(name, args, accounts, remaining)`) and decoder (accounts and events to bigint, camelCase, enum names) | `@coral-xyz/anchor` (BorshCoder), `bn.js`, `../idls/launchlab.json` |
| `accounts.ts` | `GlobalConfig`, `PlatformConfig`, `PoolState`, curve rule, allow config: types, decoders, readers; `curveState` and `curveFeeRates` for the quotes | codec, addresses, `../curve/launchlab.ts` (types) |
| `launch.ts` | `initializeV2Instruction`, `initializeWithToken2022Instruction`, `launchAddresses`, `launchedPool` (a first buy in the launch transaction) | codec, addresses |
| `trade.ts` | `tradeInstruction` (buy or sell, exact in or out, referral share fee) and `tradeInstructions` (ATAs, WSOL wrap and unwrap around it) | codec, addresses |
| `platform.ts` | `createPlatformConfigInstruction`, `updatePlatformConfigInstruction` (every variant), allow configs, curve rules, `RULE_FIELD` / `RULE_OP`, `platformParamsOf` | codec, addresses |
| `platform-reconcile.ts` | `readPlatformLive` and `platformPlan(spec, live)`: the instructions that bring a platform to a spec, empty once it matches | platform, accounts, `../../core/encoding.ts` |
| `fees.ts` | `claimPlatformFeeInstruction`, `claimCreatorFeeInstruction`, `unwrapAfterClaim` | codec, addresses |
| `migration.ts` | `migrateToCpswapInstruction` (28 + 10 accounts), `migrationProblems`, `missingMigrationAccounts`, `migratedPoolFromTransaction`, `cpmmCreatorOf` | codec, addresses, `../../core/encoding.ts` |
| `cpmm.ts` | CPMM `AmmConfig` and `PoolState` decoders, `creatorFeeShareRate`, `cpmmRates`, `cpmmSwapInstruction` (both directions), both creator-fee collects | addresses, `../curve/cpmm.ts` |
| `feekey.ts` | LP-Lock position decoder, `findFeeKeys`, `readFeeKey`, `collectFeeKeyInstructions` | addresses, cpmm (types) |
| `events.ts` | `launchlabEvents` / `tradeEvents` (TradeEvent, PoolCreateEvent from inner instructions), `cpmmSwapEvents` (SwapEvent from logs) | codec, `../../core/encoding.ts` |
| `fee-snapshot.ts` | `readFeeSnapshot`: both curve fee vaults, the CPMM creator fee net of Raydium's share, the Fee Key harvest, in one bank read | cpmm, feekey, `../curve/cpmm.ts` |

Copy the folder with `../idls/launchlab.json`, `../curve/` and `../../core/encoding.ts`. The project's
tsconfig needs `resolveJsonModule` (Next.js has it). Packages: `@solana/web3.js`, `@solana/spl-token`,
`@coral-xyz/anchor`, `bn.js` (and `@types/bn.js`). In a browser, anchor and spl-token need a global `Buffer`.

## Assembly

```ts
// Launch with the creator's first buy, in one transaction.
import { Keypair } from '@solana/web3.js'
import { globalConfigAddress, platformConfigAddress, RAYDIUM } from './engines/launchpad/launchlab/addresses'
import { readGlobalConfig, readPlatformConfig, curveFeeRates } from './engines/launchpad/launchlab/accounts'
import { initializeV2Instruction, launchedPool, type LaunchInput } from './engines/launchpad/launchlab/launch'
import { tradeInstructions } from './engines/launchpad/launchlab/trade'
import { buyExactIn, launchState } from './engines/launchpad/curve/launchlab'
import { minimumOut } from './engines/launchpad/curve/math'

const mint = Keypair.generate()
const globalConfig = globalConfigAddress(quoteMint)
const platformConfig = platformConfigAddress(platformAdmin)
const [global, platform] = await Promise.all([readGlobalConfig(connection, globalConfig), readPlatformConfig(connection, platformConfig)])

const launch: LaunchInput = {
  payer, baseMint: mint.publicKey, globalConfig, quoteMint, platformConfig, restrictions: platform,
  token: { name, symbol, uri },
  curve: { supply, totalBaseSell, totalQuoteFundRaising },
}
const quote = buyExactIn(launchState({ ...launch.curve, migrateFee: global.migrateFee }), firstBuy, curveFeeRates(global, platform))
const instructions = [
  initializeV2Instruction(launch),
  ...tradeInstructions({ payer, pool: launchedPool(launch), side: 'buy', amount: { exactIn: firstBuy, minimumOut: minimumOut(quote.base, 100) } }),
]
// Compile over RAYDIUM.lookupTable; the mint keypair signs with the payer.
```

```ts
// Trade a live pool.
const pool = await readPoolState(connection, poolAddress(mint, quoteMint))
if (pool.status === 0) {
  const q = sellExactIn(curveState(pool), amount, curveFeeRates(global, platform))
  const ixs = tradeInstructions({ payer, pool, side: 'sell', amount: { exactIn: amount, minimumOut: minimumOut(q.quote, 100) } })
}
```

```ts
// Keep a platform at its spec: run the plan to a fixed point.
for (;;) {
  const live = await readPlatformLive(connection, spec.admin, spec.globalConfigs.map((g) => g.address))
  const plan = platformPlan(spec, live)
  if (!plan.length) break
  await send(plan) // the tx engine; the admin (and the curve rule manager) sign
}
```

```ts
// After graduation: the pool from its migration, then the creator fee and the Fee Key.
const cpmmPool = migratedPoolFromTransaction(migrationTx, pool.address)!
const snap = await readFeeSnapshot(connection, { platformConfig, creator: pool.creator, quoteMint, cpmmPool, feeNftMint })
const ixs = [
  ...(snap.cpmm!.creatorFees0 + snap.cpmm!.creatorFees1 > 0n ? [collectCreatorFeePermissionlessInstruction(payer, snap.cpmm!.pool)] : []),
  ...(snap.cpmm!.feeKey?.lp ? collectFeeKeyInstructions({ owner: platform.platformNftWallet, pool: snap.cpmm!.pool, feeNftMint }) : []),
]
```

## Behaviour worth knowing

- A GlobalConfig is the PDA `["global_config", quote_mint, curve_type, index]`, one per quote mint,
  and only Raydium creates them. Every live one is constant product, curve type 0, index 0. The
  launch takes no quote-mint argument: the GlobalConfig decides it.
- Every new launch graduates to CPMM (`migrate_type` 1). LaunchLab mints the whole supply into the
  base vault and revokes the mint authority inside the launch instruction.
- The pool's creator is any key and never signs, so a program PDA can be the creator of record and
  claim through `invoke_signed`. A creator's (and a platform's) curve fees from all its pools in one
  quote mint accrue in one vault: `[creator, quote_mint]`, `[platform_config, quote_mint]`.
- A trade names, after the IDL's fifteen accounts, the share fee receiver's quote ATA (only with a
  share fee), the System program, the platform fee vault and the creator fee vault. The first trade
  into a vault opens it at the trader's cost.
- A platform that restricts GlobalConfigs or curve parameters makes every launch name its allow
  config and curve rule; pass the decoded PlatformConfig as `restrictions` and the builder does it.
- `create_platform_config` refuses a nonzero `creatorScale`, a platform fee above 50,000 (5%) and a
  creator fee above 5,000 (0.5%); the IDL says none of this. The admin pays 944 bytes of rent, so a
  PDA admin is funded first. One platform per admin key: a platform per coin means an admin per coin.
- `creatorFeeRate` and `transferFeeExtensionAuth` change only through AllInfo, which rewrites every
  field and takes the cpswap config as its third account. Build it from `platformParamsOf(live)`.
- `initialize_v2` plus a first buy fits one transaction only over `RAYDIUM.lookupTable`; adding
  `create_platform_config` does not fit, so a new platform confirms in a transaction of its own first.
- The curve completes (status 1) when its base is sold out, not when `realQuote` reaches
  `totalQuoteFundRaising`. `completionQuote` sizes the finishing buy from the remaining base.
- Only `GlobalConfig.migrate_to_cpswap_wallet` can migrate: `RAYDIUM.migrationCrank` on every mainnet
  config. A localnet crank runs on a cloned GlobalConfig whose bytes 211..243 are rewritten to its
  key (check they held the crank first). The Fee Key mint is a fresh keypair that co-signs.
- The migration's ten extra accounts end with the support-mint PDAs of the two mints in CPMM's
  sorted order, not base then quote. The nft wallet and the pool creator are writable and the
  permission PDA read-only, as on every captured mainnet migration. A platform that burns all its LP
  still passes all ten.
- The graduated pool's CPMM creator is `platform_cp_creator` when the platform set one, else the
  launch creator; code that recognizes graduated pools accepts either, and LaunchLab's vault
  authority too. Read the pool's address from its migration: never infer it from the mints, since
  anyone can create another CPMM pool for the same pair and tier.
- CPMM keeps `floor(booked × share / 1e6)` of a creator-fee collect. The share is the creator's own
  CreatorFeeShare account when Raydium set one, else the AmmConfig's rate, stored in what the public
  IDL calls padding. Read tiers and rates from the AmmConfigs on chain, not from Raydium's API.
- The permissionless creator-fee collect fails on an empty bucket: send it only when a booked fee is
  nonzero. The creator-signed collect takes the creator_fee_share account, one more than the public IDL.
- A CPMM swap charges the creator fee on its input when the pool's `creator_fee_on` is both tokens
  or the input's own token, else on its output: a coin sale on a quote-side fee pays it in the quote.
- A migrated pool opens a second after its migration (`open_time` is the migration's block time
  plus one), and CPMM answers `NotApproved` to swaps before that or while the pool's swap bit (4)
  is set. Read the Clock sysvar with the pool and treat it as still migrating until then.
- Anyone can lock dust LP of a graduated pool and name any wallet as its NFT owner. `findFeeKeys` and
  `isMigrationFeeKey` accept only locks whose `locked_owner` is LaunchLab's vault authority.
- A Fee Key harvest asks for `u64::MAX` (LP-Lock pays what has accrued) and pays both tokens. It fails
  inside CPMM when withdraw is off or one side floors to zero, so anything that must always succeed
  runs with the harvest optional; `readFeeSnapshot` reports zero for such a harvest.
- LaunchLab and CPMM pay fees as WSOL into token accounts. Closing the WSOL ATA unwraps them along
  with its rent; the trade builder recreates the WSOL ATA idempotently every time, even if a flow
  just closed it.
- Read a pool, its vaults and its fee counters in one `getMultipleAccountsInfo`, so they come from
  one bank: reserves are vault balances less the fees the pool still holds.
- Jupiter routes LaunchLab curves (label `Raydium Launchlab`), so a coin on its curve can also be
  bought through Jupiter; the LaunchLab leg takes the taker as its writable signing payer.
- In Node's ESM loader, anchor's CommonJS build does not expose `BN` as a named export; `codec.ts`
  takes `BN` from `bn.js`.
